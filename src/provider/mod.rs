//! Provider-agnostic message format, provider resolution, streaming retries,
//! and provider-specific request/response translation.

pub mod anthropic;
pub mod codex;
pub mod openai;

mod http;
mod resolution;
mod streaming;
mod types;

pub use resolution::resolve;
pub use streaming::{StreamNotice, TurnOutput, stream_turn};
pub use types::{
    Event, Message, Provider, Reasoning, ReasoningKind, Request, Role, ToolCall, ToolSpec,
};

pub(crate) use http::{SseEvent, SseReader, error_body, http_agent};
pub(crate) use streaming::append_reasoning;
