//! Minimal HTTP/1.1 request parsing and response writing for the remote
//! page. Connections stay open between requests so each keystroke costs one
//! round trip instead of a new TCP handshake.

use std::io::{self, BufRead, Read, Write};

const MAX_HEAD_BYTES: usize = 16 * 1024;
pub(super) const MAX_BODY_BYTES: usize = 256 * 1024;

pub(super) struct Request {
    pub(super) method: String,
    pub(super) path: String,
    query: String,
    headers: Vec<(String, String)>,
    pub(super) body: Vec<u8>,
}

impl Request {
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// A query parameter's raw value. Callers only compare values with
    /// URL-safe characters, so no percent-decoding is done.
    pub(super) fn query_param(&self, name: &str) -> Option<&str> {
        self.query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (key == name).then_some(value)
        })
    }

    pub(super) fn cookie(&self, name: &str) -> Option<&str> {
        self.header("cookie")?.split(';').find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then_some(value)
        })
    }
}

/// Reads the next request on a connection. Returns `None` when the client
/// closed the connection cleanly between requests.
pub(super) fn read_request(reader: &mut impl BufRead) -> io::Result<Option<Request>> {
    if reader.fill_buf()?.is_empty() {
        return Ok(None);
    }
    let mut head_bytes = 0usize;
    let mut line = String::new();
    let mut next_line = |reader: &mut dyn BufRead, line: &mut String| -> io::Result<()> {
        line.clear();
        let limit = MAX_HEAD_BYTES.saturating_sub(head_bytes) as u64 + 1;
        let read = reader.take(limit).read_line(line)?;
        head_bytes += read;
        if read == 0 || head_bytes > MAX_HEAD_BYTES || !line.ends_with('\n') {
            return Err(invalid("incomplete or oversized request head"));
        }
        line.truncate(line.trim_end_matches(['\r', '\n']).len());
        Ok(())
    };
    let reader: &mut dyn BufRead = reader;
    next_line(reader, &mut line)?;
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid("malformed request line"));
    };
    if !version.starts_with("HTTP/1.") {
        return Err(invalid("unsupported HTTP version"));
    }
    let method = method.to_string();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (path, query) = (path.to_string(), query.to_string());
    let mut headers = Vec::new();
    loop {
        next_line(reader, &mut line)?;
        if line.is_empty() {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid("malformed header"))?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    let mut request = Request {
        method,
        path,
        query,
        headers,
        body: Vec::new(),
    };
    let length = request
        .header("content-length")
        .map_or(Ok(0), str::parse::<usize>)
        .map_err(|_| invalid("invalid content length"))?;
    if length > MAX_BODY_BYTES {
        return Err(invalid("request body is too large"));
    }
    request.body.resize(length, 0);
    reader.read_exact(&mut request.body)?;
    Ok(Some(request))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(super) fn respond(
    output: &mut impl Write,
    status: &str,
    content_type: &str,
    extra_headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<()> {
    // A 204 response ends at its headers. A body (or Content-Length) would be
    // read as the start of the next response and make clients abandon the
    // kept-alive socket.
    let no_content = status.starts_with("204");
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\nConnection: keep-alive\r\n"
    );
    if !no_content {
        response.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (name, value) in extra_headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    let mut response = response.into_bytes();
    if !no_content {
        response.extend_from_slice(body);
    }
    // One write keeps small responses in a single packet.
    output.write_all(&response)?;
    output.flush()
}

pub(super) fn respond_status(output: &mut impl Write, status: &str) -> io::Result<()> {
    respond(
        output,
        status,
        "text/plain; charset=utf-8",
        &[],
        status.as_bytes(),
    )
}

pub(super) fn start_event_stream(output: &mut impl Write) -> io::Result<()> {
    output.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\n\
          X-Accel-Buffering: no\r\nConnection: keep-alive\r\n\r\n",
    )?;
    output.flush()
}

/// Writes one Server-Sent Event whose data is `payload` encoded as a JSON
/// string, so multi-line terminal output stays on a single `data:` line.
pub(super) fn write_event(output: &mut impl Write, name: &str, payload: &str) -> io::Result<()> {
    let data = serde_json::to_string(payload).map_err(io::Error::other)?;
    output.write_all(format!("event: {name}\ndata: {data}\n\n").as_bytes())?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_line_headers_cookies_and_body() -> io::Result<()> {
        let raw = b"POST /input?x=1&take HTTP/1.1\r\nHost: 100.64.0.1:7474\r\n\
                    Cookie: a=b; yawl_remote=abc\r\nContent-Length: 3\r\n\r\nhey";
        let request = read_request(&mut &raw[..])?.expect("request");
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/input");
        assert_eq!(request.query_param("x"), Some("1"));
        assert_eq!(request.query_param("take"), Some(""));
        assert_eq!(request.query_param("resume"), None);
        assert_eq!(request.header("HOST"), Some("100.64.0.1:7474"));
        assert_eq!(request.cookie("yawl_remote"), Some("abc"));
        assert_eq!(request.body, b"hey");
        Ok(())
    }

    #[test]
    fn rejects_truncated_and_oversized_requests() {
        assert!(read_request(&mut &b"GET / HTTP/1.1\r\nHost: x"[..]).is_err());
        let oversized = format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert!(read_request(&mut oversized.as_bytes()).is_err());
        assert!(read_request(&mut &b"GET / SPDY/3\r\n\r\n"[..]).is_err());
        let long_line = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(MAX_HEAD_BYTES));
        assert!(read_request(&mut long_line.as_bytes()).is_err());
    }

    #[test]
    fn reads_consecutive_requests_from_one_connection() -> io::Result<()> {
        let raw = b"POST /input HTTP/1.1\r\nContent-Length: 1\r\n\r\na\
                    POST /input HTTP/1.1\r\nContent-Length: 2\r\n\r\nbc";
        let mut reader = &raw[..];
        assert_eq!(read_request(&mut reader)?.expect("first").body, b"a");
        assert_eq!(read_request(&mut reader)?.expect("second").body, b"bc");
        assert!(read_request(&mut reader)?.is_none());
        Ok(())
    }

    #[test]
    fn no_content_responses_have_no_body() -> io::Result<()> {
        let mut output = Vec::new();
        respond_status(&mut output, "204 No Content")?;
        let text = String::from_utf8_lossy(&output);
        assert!(text.ends_with("\r\n\r\n"));
        assert!(!text.contains("Content-Length"));
        Ok(())
    }

    #[test]
    fn events_encode_payload_as_one_json_line() -> io::Result<()> {
        let mut output = Vec::new();
        write_event(&mut output, "frame", "a\nb\x1b[0m")?;
        assert_eq!(
            String::from_utf8_lossy(&output),
            "event: frame\ndata: \"a\\nb\\u001b[0m\"\n\n"
        );
        Ok(())
    }
}
