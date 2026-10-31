//! Process exit codes and the error kinds machine-readable output reports.
//! A calling agent branches on these instead of parsing messages.

use crate::error::Error;

pub(crate) const FAILURE: i32 = 1;
pub(crate) const USAGE: i32 = 2;
pub(crate) const CONFIG: i32 = 3;
pub(crate) const PROVIDER: i32 = 4;
pub(crate) const TIMEOUT: i32 = 124;
pub(crate) const INTERRUPTED: i32 = 130;

/// How a failure is reported: a stable kind, its exit code, and a hint
/// naming the command that fixes it, when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub(crate) kind: &'static str,
    pub(crate) code: i32,
    pub(crate) message: String,
    pub(crate) hint: Option<&'static str>,
}

impl Failure {
    /// A command line that cannot be acted on.
    pub(crate) fn usage(message: impl Into<String>) -> Self {
        failure(
            "usage",
            USAGE,
            message.into(),
            Some("run 'yawl --help' for usage"),
        )
    }

    /// `--timeout` expired before the run could start.
    pub(crate) fn timeout(message: impl Into<String>) -> Self {
        failure("timeout", TIMEOUT, message.into(), None)
    }

    /// The `status` a result object reports for this failure.
    pub(crate) fn status(&self) -> &'static str {
        match self.kind {
            "timeout" => "timeout",
            "interrupted" => "interrupted",
            _ => "error",
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

/// Classifies any error that reaches the top of a run. A [`Failure`] built
/// where the problem was found passes through unchanged.
pub(crate) fn classify(error: &(dyn std::error::Error + 'static)) -> Failure {
    if let Some(failure) = error.downcast_ref::<Failure>() {
        return failure.clone();
    }
    if let Some(error) = error.downcast_ref::<Error>() {
        return classify_error(error);
    }
    if let Some(error) = error.downcast_ref::<std::io::Error>() {
        return classify_io(error);
    }
    failure("error", FAILURE, error.to_string(), None)
}

pub(crate) fn classify_error(error: &Error) -> Failure {
    let message = error.to_string();
    match error {
        Error::Config(_) => failure(
            "config",
            CONFIG,
            message,
            Some("run 'yawl --check' to see what is missing"),
        ),
        Error::Http {
            status: 401 | 403, ..
        } => failure(
            "auth",
            CONFIG,
            message,
            Some("check the provider's API key, or run 'yawl --login openai-codex' for Codex"),
        ),
        Error::Http { status: 404, .. } => failure(
            "provider",
            PROVIDER,
            message,
            Some("the model may not exist; run 'yawl --list-models --refresh'"),
        ),
        Error::Http { .. } | Error::Protocol(_) | Error::MalformedToolCall(_) => {
            failure("provider", PROVIDER, message, None)
        }
        Error::Invalid(_) => failure("invalid", USAGE, message, None),
        Error::Interrupted => failure("interrupted", INTERRUPTED, message, None),
        Error::Io(error) => classify_io(error),
    }
}

fn classify_io(error: &std::io::Error) -> Failure {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::ConnectionRefused
        | ErrorKind::ConnectionReset
        | ErrorKind::ConnectionAborted
        | ErrorKind::NotConnected
        | ErrorKind::HostUnreachable
        | ErrorKind::NetworkUnreachable
        | ErrorKind::AddrNotAvailable => failure(
            "provider",
            PROVIDER,
            format!("cannot reach the provider: {error}"),
            Some("check that the provider's server is running and its base URL is right"),
        ),
        // Provider requests that time out or stall surface as `TimedOut`
        // (see `error.rs` and `provider/http.rs`). Only `--timeout` itself
        // reports the `timeout` kind, through `Failure::timeout`.
        ErrorKind::TimedOut => failure(
            "provider",
            PROVIDER,
            format!("the provider stopped responding: {error}"),
            Some("retry later, or pick another model with --model"),
        ),
        ErrorKind::Interrupted => failure("interrupted", INTERRUPTED, error.to_string(), None),
        _ => failure("io", FAILURE, error.to_string(), None),
    }
}

fn failure(kind: &'static str, code: i32, message: String, hint: Option<&'static str>) -> Failure {
    Failure {
        kind,
        code,
        message,
        hint,
    }
}

impl Failure {
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind,
            "message": self.message,
            "hint": self.hint,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_map_to_stable_kinds_and_codes() {
        let cases = [
            (Error::Config("no model".into()), "config", CONFIG),
            (
                Error::Http {
                    status: 401,
                    body: String::new(),
                },
                "auth",
                CONFIG,
            ),
            (
                Error::Http {
                    status: 503,
                    body: String::new(),
                },
                "provider",
                PROVIDER,
            ),
            (Error::Protocol("bad stream".into()), "provider", PROVIDER),
            (
                Error::Invalid("nothing to continue".into()),
                "invalid",
                USAGE,
            ),
            (Error::Interrupted, "interrupted", INTERRUPTED),
        ];
        for (error, kind, code) in cases {
            let failure = classify(&error);
            assert_eq!((failure.kind, failure.code), (kind, code), "{error}");
        }
    }

    #[test]
    fn io_errors_name_the_provider_or_stay_generic() {
        let refused = Error::Io(std::io::ErrorKind::ConnectionRefused.into());
        assert_eq!(classify(&refused).kind, "provider");
        assert_eq!(classify(&refused).code, PROVIDER);
        // A stalled or timed-out request is the provider's failure, not
        // `--timeout`, so a caller does not resume against a dead endpoint.
        let stalled = Error::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "connection stalled",
        ));
        assert_eq!(
            (classify(&stalled).kind, classify(&stalled).code),
            ("provider", PROVIDER)
        );
        let ureq_timeout: Error = ureq::Error::Timeout(ureq::Timeout::Global).into();
        assert_eq!(classify(&ureq_timeout).kind, "provider");
        // Other `InvalidInput` errors are plain I/O failures, not usage.
        let invalid = Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL"));
        assert_eq!(
            (classify(&invalid).kind, classify(&invalid).code),
            ("io", FAILURE)
        );
        let boxed: Box<dyn std::error::Error> = Box::new(std::io::Error::other("disk"));
        let failure = classify(boxed.as_ref());
        assert_eq!((failure.kind, failure.code), ("io", FAILURE));
    }

    #[test]
    fn built_failures_pass_through_classification() {
        let usage: Box<dyn std::error::Error> = Box::new(Failure::usage("unknown option"));
        let failure = classify(usage.as_ref());
        assert_eq!((failure.kind, failure.code), ("usage", USAGE));
        assert!(failure.hint.unwrap().contains("--help"));
        assert_eq!(failure.status(), "error");
        let timeout = Failure::timeout("stdin never closed");
        assert_eq!((timeout.code, timeout.status()), (TIMEOUT, "timeout"));
        assert_eq!(timeout.to_string(), "stdin never closed");
    }

    #[test]
    fn failure_json_carries_kind_message_and_hint() {
        let value = classify(&Error::Config("no model configured".into())).to_json();
        assert_eq!(value["kind"], "config");
        assert!(value["message"].as_str().unwrap().contains("no model"));
        assert!(value["hint"].as_str().unwrap().contains("--check"));
    }
}
