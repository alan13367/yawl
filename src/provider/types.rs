use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    /// A single streaming attempt; retries are layered on by
    /// [`crate::provider::stream_turn`].
    fn stream_once(&self, req: &Request<'_>, on_event: &mut dyn FnMut(Event)) -> Result<(), Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_roundtrips_through_json() -> Result<(), serde_json::Error> {
        let mut m = Message::assistant("ok".into(), Vec::new());
        m.reasoning.push(Reasoning {
            kind: ReasoningKind::Summary,
            content: "Checked the result".into(),
        });
        let text = serde_json::to_string(&m)?;
        let back: Message = serde_json::from_str(&text)?;
        assert_eq!(back.role, Role::Assistant);
        assert_eq!(back.reasoning, m.reasoning);
        Ok(())
    }
}
