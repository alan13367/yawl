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

/// Optional control metadata for goal, steering, and synthetic tool protocol
/// messages. Older session files omit the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageControl {
    GoalStart,
    GoalContinuation,
    Steering,
    ToolSkipped,
}

/// Metadata for one synthetic background result delivered to the parent.
/// Providers still receive the containing message as ordinary user content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentResult {
    pub id: String,
    pub name: String,
    pub status: String,
    pub run_number: u64,
    pub content: String,
}

/// One inline raster image supplied to a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageContent {
    pub media_type: String,
    /// Standard base64 without a data-URL prefix.
    pub data: String,
}

/// User-authored input for one agent turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnInput {
    pub text: String,
    pub images: Vec<ImageContent>,
}

impl From<String> for TurnInput {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
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
    pub images: Vec<ImageContent>,
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
    /// Synthetic model-originated subagent results batched into this user
    /// message. Older session files omit the field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagent_results: Vec<SubagentResult>,
    /// Marks goal and steering control messages. Older session files omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<MessageControl>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Message {
        Message {
            role: Role::User,
            content: content.into(),
            images: Vec::new(),
            tool_calls: Vec::new(),
            reasoning: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            is_error: false,
            provider_data: Vec::new(),
            subagent_results: Vec::new(),
            control: None,
        }
    }

    pub fn user_input(input: TurnInput) -> Message {
        Message {
            role: Role::User,
            content: input.text,
            images: input.images,
            tool_calls: Vec::new(),
            reasoning: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            is_error: false,
            provider_data: Vec::new(),
            subagent_results: Vec::new(),
            control: None,
        }
    }

    pub fn assistant(content: String, tool_calls: Vec<ToolCall>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            images: Vec::new(),
            tool_calls,
            reasoning: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            is_error: false,
            provider_data: Vec::new(),
            subagent_results: Vec::new(),
            control: None,
        }
    }

    pub fn tool_result(
        call_id: impl Into<String>,
        name: impl Into<String>,
        content: String,
        is_error: bool,
    ) -> Message {
        Self::tool_result_with_images(call_id, name, content, Vec::new(), is_error)
    }

    pub fn tool_result_with_images(
        call_id: impl Into<String>,
        name: impl Into<String>,
        content: String,
        images: Vec<ImageContent>,
        is_error: bool,
    ) -> Message {
        Message {
            role: Role::Tool,
            content,
            images,
            tool_calls: Vec::new(),
            reasoning: Vec::new(),
            tool_call_id: Some(call_id.into()),
            tool_name: Some(name.into()),
            is_error,
            provider_data: Vec::new(),
            subagent_results: Vec::new(),
            control: None,
        }
    }

    pub fn with_control(mut self, control: MessageControl) -> Message {
        self.control = Some(control);
        self
    }

    pub fn is_hidden_control(&self) -> bool {
        matches!(self.control, Some(MessageControl::GoalContinuation))
    }

    pub fn is_steering(&self) -> bool {
        matches!(self.control, Some(MessageControl::Steering))
    }

    pub fn is_goal_start(&self) -> bool {
        matches!(self.control, Some(MessageControl::GoalStart))
    }

    pub fn is_skipped_tool(&self) -> bool {
        matches!(self.control, Some(MessageControl::ToolSkipped))
    }

    pub fn subagent_results(results: Vec<SubagentResult>) -> Message {
        let mut content = String::from("Background subagent results:\n");
        for result in &results {
            content.push_str(&format!(
                "\n## {} [{}] {}\n{}\n",
                result.id, result.status, result.name, result.content
            ));
        }
        Message {
            role: Role::User,
            content,
            images: Vec::new(),
            tool_calls: Vec::new(),
            reasoning: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            is_error: false,
            provider_data: Vec::new(),
            subagent_results: results,
            control: None,
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
    /// Whether this request's selected model accepts image inputs.
    pub supports_images: bool,
}

/// Events surfaced by a provider while streaming one assistant response.
#[derive(Debug)]
pub enum Event {
    TextDelta(String),
    ReasoningDelta {
        kind: ReasoningKind,
        text: String,
    },
    /// The accumulated tool name, emitted while a call's arguments are still streaming.
    ToolCallName(String),
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

    #[test]
    fn old_messages_default_to_no_images() -> Result<(), serde_json::Error> {
        let message: Message = serde_json::from_str(r#"{"role":"user","content":"hello"}"#)?;
        assert!(message.images.is_empty());
        assert_eq!(message.control, None);
        Ok(())
    }

    #[test]
    fn image_messages_roundtrip_inline_data() -> Result<(), serde_json::Error> {
        let message = Message::user_input(TurnInput {
            text: "[Image #1]".into(),
            images: vec![ImageContent {
                media_type: "image/png".into(),
                data: "aW1hZ2U=".into(),
            }],
        });
        let serialized = serde_json::to_string(&message)?;
        let replayed: Message = serde_json::from_str(&serialized)?;
        assert_eq!(replayed.images, message.images);
        Ok(())
    }
}
