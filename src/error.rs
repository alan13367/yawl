use std::fmt;

/// Crate-wide error type. Hand-rolled: the dependency policy excludes
/// thiserror/anyhow.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// Non-2xx HTTP response from a provider.
    Http {
        status: u16,
        body: String,
    },
    /// Malformed wire data, unexpected stream shape, transport errors.
    Protocol(String),
    Config(String),
    /// The user aborted the in-flight turn with Ctrl+C.
    Interrupted,
}

impl Error {
    /// Whether the retry wrapper should re-attempt the request:
    /// 429/5xx statuses and I/O failures (connect errors, mid-stream
    /// disconnects).
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Http { status, .. } => *status == 429 || *status >= 500,
            Error::Io(_) => true,
            _ => false,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "i/o error: {e}"),
            Error::Http { status, body } => {
                let body = body.trim();
                if body.is_empty() {
                    write!(f, "http {status}")
                } else {
                    write!(f, "http {status}: {}", truncate(body, 400))
                }
            }
            Error::Protocol(msg) => write!(f, "protocol error: {msg}"),
            Error::Config(msg) => write!(f, "config error: {msg}"),
            Error::Interrupted => write!(f, "interrupted"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Protocol(format!("bad json: {e}"))
    }
}

impl From<ureq::Error> for Error {
    fn from(e: ureq::Error) -> Self {
        match e {
            ureq::Error::Io(io) => Error::Io(io),
            ureq::Error::Timeout(_) => Error::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "request timed out",
            )),
            other => Error::Protocol(other.to_string()),
        }
    }
}

pub(crate) fn truncate(s: &str, max_chars: usize) -> String {
    let Some((cut, _)) = s.char_indices().nth(max_chars) else {
        return s.to_string();
    };
    format!("{}…", &s[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_character_boundaries_and_exact_limits() {
        assert_eq!(truncate("éclair", 2), "éc…");
        assert_eq!(truncate("éclair", 6), "éclair");
        assert_eq!(truncate("éclair", 0), "…");
        assert_eq!(truncate("", 0), "");
    }
}
