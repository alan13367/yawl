use std::io::{BufRead, Take};
use std::time::Duration;

use crate::error::Error;

fn read_line_interruptible(reader: &mut impl BufRead, line: &mut String) -> std::io::Result<usize> {
    let mut bytes = Vec::new();
    loop {
        let (consumed, finished) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                break;
            }
            let consumed = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |index| index + 1);
            bytes.extend_from_slice(&available[..consumed]);
            (consumed, available[consumed - 1] == b'\n')
        };
        reader.consume(consumed);
        if finished {
            break;
        }
    }
    let read = bytes.len();
    let text = String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    line.push_str(&text);
    Ok(read)
}

/// Shared ureq agent config for streaming: no global timeout (streams are
/// long-lived), non-2xx statuses surfaced as responses so we can read error
/// bodies.
pub(crate) fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(None)
        .timeout_connect(Some(Duration::from_secs(20)))
        .build()
        .into()
}

/// One server-sent event: the `event:` name (may be empty) and the joined
/// `data:` payload.
pub(crate) struct SseEvent {
    pub event: String,
    pub data: String,
}

/// Ceiling for one SSE event's joined `data:` payload. Real provider events
/// stay far below this; the cap stops a runaway server from ballooning
/// memory.
pub(crate) const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;
/// Ceiling for one streaming response's total SSE bytes.
pub(crate) const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;

/// Hand-rolled SSE parser over a blocking reader. Yields one event per
/// blank-line-terminated block; checks the interrupt flag between reads.
/// Total bytes are bounded via `Take`, so even a single unterminated line
/// cannot allocate past the response ceiling.
pub(crate) struct SseReader<R> {
    reader: Take<R>,
    event_limit: usize,
}

impl<R: BufRead> SseReader<R> {
    pub fn new(reader: R) -> Self {
        Self::with_limits(reader, MAX_EVENT_BYTES, MAX_RESPONSE_BYTES)
    }

    pub(crate) fn with_limits(reader: R, event_limit: usize, response_limit: u64) -> Self {
        SseReader {
            reader: reader.take(response_limit),
            event_limit,
        }
    }
}

impl<R: BufRead> Iterator for SseReader<R> {
    type Item = Result<SseEvent, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut event = String::new();
        let mut data = String::new();
        let mut saw_field = false;
        let mut line = String::new();
        loop {
            if crate::cancellation::interrupted() {
                return Some(Err(Error::Interrupted));
            }
            line.clear();
            match read_line_interruptible(&mut self.reader, &mut line) {
                Ok(0) => {
                    // `Take` reports EOF at the byte ceiling; distinguish
                    // that from a genuine end of stream so oversized
                    // responses fail loudly instead of truncating silently.
                    if self.reader.limit() == 0 {
                        return Some(Err(Error::Protocol(format!(
                            "response exceeded {MAX_RESPONSE_BYTES} SSE bytes"
                        ))));
                    }
                    return if saw_field {
                        Some(Ok(SseEvent { event, data }))
                    } else {
                        None
                    };
                }
                Ok(_) => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::Interrupted
                        && crate::cancellation::interrupted() =>
                {
                    return Some(Err(Error::Interrupted));
                }
                Err(e) => return Some(Err(Error::Io(e))),
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                if saw_field {
                    return Some(Ok(SseEvent { event, data }));
                }
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("event:") {
                event = rest.trim_start().to_string();
                saw_field = true;
            } else if let Some(rest) = trimmed.strip_prefix("data:") {
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                if data.len() + rest.len() > self.event_limit {
                    return Some(Err(Error::Protocol(format!(
                        "SSE event exceeded {} bytes",
                        self.event_limit
                    ))));
                }
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest);
                saw_field = true;
            }
            // Comment lines (":...") and unknown fields are ignored.
        }
    }
}

/// Reads a non-2xx response body (bounded) for error reporting.
pub(crate) fn error_body(response: &mut ureq::http::Response<ureq::Body>) -> String {
    response
        .body_mut()
        .with_config()
        .limit(64 * 1024)
        .read_to_string()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

    struct WokenReader(crate::cancellation::CancellationToken);

    impl Read for WokenReader {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            self.0.cancel();
            Err(std::io::ErrorKind::Interrupted.into())
        }
    }

    impl BufRead for WokenReader {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            self.0.cancel();
            Err(std::io::ErrorKind::Interrupted.into())
        }

        fn consume(&mut self, _amount: usize) {}
    }

    #[test]
    fn sse_reader_parses_events_and_multiline_data() -> Result<(), Error> {
        let input =
            "event: message_start\ndata: {\"a\":1}\n\n: comment\ndata: line1\ndata: line2\n\n";
        let mut r = SseReader::new(Cursor::new(input));
        let Some(e1) = r.next() else {
            panic!("fixture should contain a first SSE event");
        };
        let e1 = e1?;
        assert_eq!(e1.event, "message_start");
        assert_eq!(e1.data, "{\"a\":1}");
        let Some(e2) = r.next() else {
            panic!("fixture should contain a second SSE event");
        };
        let e2 = e2?;
        assert_eq!(e2.event, "");
        assert_eq!(e2.data, "line1\nline2");
        assert!(r.next().is_none());
        Ok(())
    }

    #[test]
    fn sse_reader_rejects_oversized_events() {
        let oversized = "x".repeat(64);
        let input = format!("data: {oversized}\n\n");
        let mut r = SseReader::with_limits(Cursor::new(input), 16, 1024);

        let error = r.next().expect("oversized event should yield an error");

        let Err(Error::Protocol(message)) = error else {
            panic!("oversized event should be a protocol error");
        };
        assert!(message.contains("SSE event"));
    }

    #[test]
    fn sse_reader_rejects_responses_over_the_byte_ceiling() {
        let input = format!("data: one\n\ndata: {}\n\ndata: tail\n\n", "y".repeat(64));
        let mut r = SseReader::with_limits(Cursor::new(input), 1024, 80);

        let mut events = Vec::new();
        loop {
            match r.next() {
                Some(Ok(event)) => events.push(event),
                Some(Err(Error::Protocol(msg))) => {
                    assert!(msg.contains("response exceeded"));
                    break;
                }
                Some(Err(error)) => panic!("unexpected error: {error}"),
                None => panic!("over-ceiling response should error, not end cleanly"),
            }
        }
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn a_targeted_wake_maps_interrupted_io_to_turn_interruption() {
        crate::set_interrupted(false);
        let token = crate::cancellation::CancellationToken::default();
        crate::cancellation::scope(&token, || {
            let mut reader = SseReader::new(WokenReader(token.clone()));
            assert!(matches!(reader.next(), Some(Err(Error::Interrupted))));
        });
    }
}
