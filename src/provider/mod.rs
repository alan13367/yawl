//! Provider-agnostic message format, the `Provider` trait, a hand-rolled SSE
//! reader, and the retry wrapper. Per-provider request/response translation
//! lives in `anthropic.rs` and `openai.rs`.

pub mod anthropic;
pub mod codex;
pub mod openai;

use std::io::BufRead;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;
use crate::error::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    /// A tool result, paired with an assistant tool call via `tool_call_id`.
    Tool,
}

/// How much of a model's reasoning a provider exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningKind {
    /// A short provider-generated description, such as a Codex summary.
    Summary,
    /// The model's full reasoning stream, as exposed by local models.
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reasoning {
    pub kind: ReasoningKind,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON text as produced by the model. Kept unparsed so malformed
    /// arguments can be reported back to the model as a tool error.
    pub arguments: String,
}

/// The provider-agnostic message format used in memory and in session JSONL.
/// Translated to each provider's wire shape at request time, which is what
/// makes mid-session `/model` switching possible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Displayable reasoning returned alongside this assistant message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning: Vec<Reasoning>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
    /// Provider-specific replay data. Codex stores encrypted reasoning items
    /// here so `store: false` tool loops remain valid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_data: Vec<Value>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Message {
        Message {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            reasoning: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            is_error: false,
            provider_data: Vec::new(),
        }
    }

    pub fn assistant(content: String, tool_calls: Vec<ToolCall>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            tool_calls,
            reasoning: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            is_error: false,
            provider_data: Vec::new(),
        }
    }

    pub fn tool_result(
        call_id: impl Into<String>,
        name: impl Into<String>,
        content: String,
        is_error: bool,
    ) -> Message {
        Message {
            role: Role::Tool,
            content,
            tool_calls: Vec::new(),
            reasoning: Vec::new(),
            tool_call_id: Some(call_id.into()),
            tool_name: Some(name.into()),
            is_error,
            provider_data: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// One streaming request. Providers translate this to their wire format.
pub struct Request<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    pub max_tokens: u32,
}

/// Events surfaced by a provider while streaming one assistant response.
#[derive(Debug)]
pub enum Event {
    TextDelta(String),
    ReasoningDelta {
        kind: ReasoningKind,
        text: String,
    },
    /// A complete tool call (emitted once its arguments finished streaming).
    ToolCall(ToolCall),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// Opaque data needed to replay a provider response on the next request.
    ProviderData(Value),
    Done,
}

pub trait Provider {
    /// A single streaming attempt; retries are layered on by [`stream_turn`].
    fn stream_once(&self, req: &Request<'_>, on_event: &mut dyn FnMut(Event)) -> Result<(), Error>;
}

/// Accumulated result of one assistant response.
#[derive(Debug, Default)]
pub struct TurnOutput {
    pub text: String,
    pub reasoning: Vec<Reasoning>,
    pub tool_calls: Vec<ToolCall>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub provider_data: Vec<Value>,
}

/// Out-of-band notices from the retry wrapper, for display.
pub enum StreamNotice<'a> {
    TextDelta(&'a str),
    ReasoningDelta {
        kind: ReasoningKind,
        text: &'a str,
    },
    /// A retry is about to restart the request from scratch; the consumer
    /// must discard any partial text it displayed.
    RetryReset,
    Retrying {
        attempt: u32,
        delay_ms: u64,
        error: String,
    },
}

const MAX_ATTEMPTS: u32 = 3;

/// Streams one assistant response with retries: exponential backoff, up to
/// 3 attempts, on 429/5xx and I/O failures (including mid-stream
/// disconnects). A retry restarts the whole request; `RetryReset` tells the
/// consumer to drop partial output.
pub fn stream_turn(
    provider: &dyn Provider,
    req: &Request<'_>,
    sink: &mut dyn FnMut(StreamNotice<'_>),
) -> Result<TurnOutput, Error> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut out = TurnOutput::default();
        let result = provider.stream_once(req, &mut |event| match event {
            Event::TextDelta(t) => {
                sink(StreamNotice::TextDelta(&t));
                out.text.push_str(&t);
            }
            Event::ReasoningDelta { kind, text } => {
                sink(StreamNotice::ReasoningDelta { kind, text: &text });
                append_reasoning(&mut out.reasoning, kind, &text);
            }
            Event::ToolCall(tc) => out.tool_calls.push(tc),
            Event::Usage {
                input_tokens,
                output_tokens,
            } => {
                out.input_tokens = input_tokens;
                out.output_tokens = output_tokens;
            }
            Event::ProviderData(value) => out.provider_data.push(value),
            Event::Done => {}
        });
        match result {
            Ok(()) => return Ok(out),
            Err(_) if crate::interrupted() => return Err(Error::Interrupted),
            Err(e) if e.is_retryable() && attempt < MAX_ATTEMPTS => {
                let delay_ms = 500u64 << (attempt - 1);
                sink(StreamNotice::Retrying {
                    attempt,
                    delay_ms,
                    error: e.to_string(),
                });
                std::thread::sleep(Duration::from_millis(delay_ms));
                if crate::interrupted() {
                    return Err(Error::Interrupted);
                }
                sink(StreamNotice::RetryReset);
            }
            Err(e) => return Err(e),
        }
    }
}

pub(crate) fn append_reasoning(reasoning: &mut Vec<Reasoning>, kind: ReasoningKind, text: &str) {
    if let Some(current) = reasoning.last_mut()
        && current.kind == kind
    {
        current.content.push_str(text);
    } else {
        reasoning.push(Reasoning {
            kind,
            content: text.to_string(),
        });
    }
}

/// Resolves a model spec to a provider instance and the bare model name.
///
/// Explicit `anthropic:` and `openai:` prefixes select the built-ins. Any
/// prefix found in `config.providers` selects that OpenAI-compatible
/// provider. Otherwise, names starting with `claude` use Anthropic and all
/// other names use the built-in OpenAI endpoint.
pub fn resolve(model_spec: &str, cfg: &Config) -> Result<(Box<dyn Provider>, String), Error> {
    let target = crate::model::ModelTarget::parse(model_spec, cfg);
    let bare = target.model();
    match target.provider() {
        crate::model::ProviderSelection::Anthropic => anthropic_provider(cfg, bare),
        crate::model::ProviderSelection::OpenAi => openai_provider(cfg, bare),
        crate::model::ProviderSelection::Codex => {
            Ok((Box::new(codex::Codex::from_config(cfg)?), bare.to_string()))
        }
        crate::model::ProviderSelection::Custom {
            name,
            config: provider,
        } => custom_provider(name, provider, bare),
    }
}

fn custom_provider(
    name: &str,
    provider: &crate::config::ProviderConfig,
    model: &str,
) -> Result<(Box<dyn Provider>, String), Error> {
    if provider.api != "openai-completions" {
        return Err(Error::Config(format!(
            "provider '{name}' uses unsupported API '{}'; Yawl supports openai-completions",
            provider.api
        )));
    }
    if provider.base_url.trim().is_empty() {
        return Err(Error::Config(format!("provider '{name}' has no base_url")));
    }

    let key = match &provider.api_key {
        Some(value) => crate::config::resolve_config_value(value)?,
        None => std::env::var(provider_key_environment_name(name)).unwrap_or_default(),
    };
    let mut headers = Vec::with_capacity(provider.headers.len());
    for (header_name, value) in &provider.headers {
        let value = crate::config::resolve_config_value(value)?;
        validate_header(header_name, &value)?;
        headers.push((header_name.clone(), value));
    }
    headers.sort_by(|left, right| left.0.cmp(&right.0));

    let mut compat = provider.compat.clone();
    if let Some(model) = provider
        .models
        .iter()
        .find(|candidate| candidate.id == model)
    {
        compat.apply(model.compat.clone());
    }
    if !matches!(
        compat.max_tokens_field(),
        "max_tokens" | "max_completion_tokens"
    ) {
        return Err(Error::Config(format!(
            "provider '{name}' has unsupported maxTokensField '{}'",
            compat.max_tokens_field()
        )));
    }
    Ok((
        Box::new(openai::OpenAi::configured(
            provider.base_url.clone(),
            key,
            provider.auth_header.unwrap_or(true),
            headers,
            compat,
        )),
        model.to_string(),
    ))
}

fn anthropic_provider(cfg: &Config, model: &str) -> Result<(Box<dyn Provider>, String), Error> {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| Error::Config("ANTHROPIC_API_KEY is not set".into()))?;
    Ok((
        Box::new(anthropic::Anthropic::new(
            cfg.anthropic_base_url.clone(),
            key,
        )),
        model.to_string(),
    ))
}

fn openai_provider(cfg: &Config, model: &str) -> Result<(Box<dyn Provider>, String), Error> {
    let key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
    Ok((
        Box::new(openai::OpenAi::new(cfg.openai_base_url.clone(), key)),
        model.to_string(),
    ))
}

fn provider_key_environment_name(provider: &str) -> String {
    let mut name = provider
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    name.push_str("_API_KEY");
    name
}

fn validate_header(name: &str, value: &str) -> Result<(), Error> {
    name.parse::<ureq::http::HeaderName>()
        .map_err(|error| Error::Config(format!("invalid provider header '{name}': {error}")))?;
    value.parse::<ureq::http::HeaderValue>().map_err(|error| {
        Error::Config(format!(
            "invalid value for provider header '{name}': {error}"
        ))
    })?;
    Ok(())
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

/// Hand-rolled SSE parser over a blocking reader. Yields one event per
/// blank-line-terminated block; checks the interrupt flag between reads.
pub(crate) struct SseReader<R> {
    reader: R,
}

impl<R: BufRead> SseReader<R> {
    pub fn new(reader: R) -> Self {
        SseReader { reader }
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
            if crate::interrupted() {
                return Some(Err(Error::Interrupted));
            }
            line.clear();
            match self.reader.read_line(&mut line) {
                Ok(0) => {
                    return if saw_field {
                        Some(Ok(SseEvent { event, data }))
                    } else {
                        None
                    };
                }
                Ok(_) => {}
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
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
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
    use std::cell::Cell;
    use std::io::Cursor;

    #[test]
    fn sse_reader_parses_events_and_multiline_data() {
        let input =
            "event: message_start\ndata: {\"a\":1}\n\n: comment\ndata: line1\ndata: line2\n\n";
        let mut r = SseReader::new(Cursor::new(input));
        let e1 = r.next().unwrap().unwrap();
        assert_eq!(e1.event, "message_start");
        assert_eq!(e1.data, "{\"a\":1}");
        let e2 = r.next().unwrap().unwrap();
        assert_eq!(e2.event, "");
        assert_eq!(e2.data, "line1\nline2");
        assert!(r.next().is_none());
    }

    #[test]
    fn message_roundtrips_through_json() {
        let mut m = Message::assistant("ok".into(), Vec::new());
        m.reasoning.push(Reasoning {
            kind: ReasoningKind::Summary,
            content: "Checked the result".into(),
        });
        let text = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&text).unwrap();
        assert_eq!(back.role, Role::Assistant);
        assert_eq!(back.reasoning, m.reasoning);
    }

    struct FlakyProvider {
        calls: Cell<u32>,
    }

    impl Provider for FlakyProvider {
        fn stream_once(
            &self,
            _req: &Request<'_>,
            on_event: &mut dyn FnMut(Event),
        ) -> Result<(), Error> {
            let call = self.calls.get() + 1;
            self.calls.set(call);
            if call == 1 {
                on_event(Event::ReasoningDelta {
                    kind: ReasoningKind::Full,
                    text: "partial thought".into(),
                });
                on_event(Event::TextDelta("partial".into()));
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "disconnected",
                )));
            }
            on_event(Event::ReasoningDelta {
                kind: ReasoningKind::Full,
                text: "complete thought".into(),
            });
            on_event(Event::TextDelta("complete".into()));
            on_event(Event::Usage {
                input_tokens: 10,
                output_tokens: 2,
            });
            on_event(Event::Done);
            Ok(())
        }
    }

    #[test]
    fn retries_discard_partial_attempt_output() {
        crate::set_interrupted(false);
        let provider = FlakyProvider {
            calls: Cell::new(0),
        };
        let request = Request {
            model: "test",
            system: "",
            messages: &[],
            tools: &[],
            max_tokens: 10,
        };
        let mut notices = Vec::new();
        let output = stream_turn(&provider, &request, &mut |notice| match notice {
            StreamNotice::TextDelta(text) => notices.push(format!("text:{text}")),
            StreamNotice::ReasoningDelta { kind, text } => {
                notices.push(format!("reasoning:{kind:?}:{text}"));
            }
            StreamNotice::RetryReset => notices.push("reset".into()),
            StreamNotice::Retrying { attempt, .. } => {
                notices.push(format!("retry:{attempt}"));
            }
        })
        .expect("second attempt should succeed");
        assert_eq!(provider.calls.get(), 2);
        assert_eq!(output.text, "complete");
        assert_eq!(output.reasoning[0].content, "complete thought");
        assert_eq!(
            notices,
            [
                "reasoning:Full:partial thought",
                "text:partial",
                "retry:1",
                "reset",
                "reasoning:Full:complete thought",
                "text:complete"
            ]
        );
        assert_eq!((output.input_tokens, output.output_tokens), (10, 2));
    }
}
