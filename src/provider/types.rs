use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Error;

const OPENAI_PROMPT_CACHE_KEY_MAX_CHARS: usize = 64;

pub(super) fn clamp_openai_prompt_cache_key(key: Option<&str>) -> Option<Cow<'_, str>> {
    key.map(|key| {
        key.char_indices()
            .nth(OPENAI_PROMPT_CACHE_KEY_MAX_CHARS)
            .map_or(Cow::Borrowed(key), |(end, _)| Cow::Owned(key[..end].into()))
    })
}

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

/// Normalized token accounting for one completed provider request.
///
/// `input_tokens` is the full logical prompt size, including cache reads and
/// writes. The cache fields are subsets of that total when the provider
/// reports them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    /// The provider included cache-specific usage details, even if both
    /// reported counts were zero.
    pub cache_details_reported: bool,
}

impl TokenUsage {
    pub fn total_tokens(self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    pub fn fresh_input_tokens(self) -> u64 {
        self.input_tokens
            .saturating_sub(self.cached_input_tokens)
            .saturating_sub(self.cache_write_input_tokens)
    }

    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_add(other.cached_input_tokens),
            cache_write_input_tokens: self
                .cache_write_input_tokens
                .saturating_add(other.cache_write_input_tokens),
            cache_details_reported: self.cache_details_reported || other.cache_details_reported,
        }
    }
}

/// Token and prompt-cache totals accumulated across a conversation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UsageSummary {
    pub requests: u64,
    pub tokens: TokenUsage,
    /// Input tokens from requests that supplied cache-specific usage. This is
    /// the denominator for cache-hit percentages when a session mixes
    /// providers with different reporting support.
    pub cache_reported_input_tokens: u64,
    pub cache_resets: u64,
}

impl UsageSummary {
    pub fn record(&mut self, usage: TokenUsage) {
        self.requests = self.requests.saturating_add(1);
        if usage.cache_details_reported {
            self.cache_reported_input_tokens = self
                .cache_reported_input_tokens
                .saturating_add(usage.input_tokens);
        }
        self.tokens = self.tokens.saturating_add(usage);
    }

    pub fn record_cache_reset(&mut self) {
        self.cache_resets = self.cache_resets.saturating_add(1);
    }

    pub fn merge(&mut self, other: Self) {
        self.requests = self.requests.saturating_add(other.requests);
        self.tokens = self.tokens.saturating_add(other.tokens);
        self.cache_reported_input_tokens = self
            .cache_reported_input_tokens
            .saturating_add(other.cache_reported_input_tokens);
        self.cache_resets = self.cache_resets.saturating_add(other.cache_resets);
    }

    pub fn cache_hit_percent(self) -> u64 {
        if self.cache_reported_input_tokens == 0 {
            return 0;
        }
        ((u128::from(self.tokens.cached_input_tokens) * 100)
            / u128::from(self.cache_reported_input_tokens))
        .min(100) as u64
    }
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
    /// Whether providers should add explicit prompt-cache controls and
    /// routing hints. Providers with implicit caching may still cache the
    /// request when this is false.
    pub prompt_cache_control: bool,
    /// Stable session identifier used only by providers that support prompt
    /// cache routing. Other providers leave the wire request unchanged.
    pub prompt_cache_key: Option<&'a str>,
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
    Usage(TokenUsage),
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
    fn old_usage_records_default_to_no_cache_details() -> Result<(), serde_json::Error> {
        let usage: TokenUsage = serde_json::from_str(r#"{"input_tokens":10,"output_tokens":2}"#)?;
        assert_eq!(usage.total_tokens(), 12);
        assert!(!usage.cache_details_reported);
        Ok(())
    }

    #[test]
    fn openai_cache_keys_are_clamped_to_64_characters() {
        let unicode = "å".repeat(65);
        assert_eq!(
            clamp_openai_prompt_cache_key(Some(&unicode)).as_deref(),
            Some("å".repeat(64).as_str())
        );
        assert_eq!(
            clamp_openai_prompt_cache_key(Some("short")).as_deref(),
            Some("short")
        );
        assert!(clamp_openai_prompt_cache_key(None).is_none());
    }

    #[test]
    fn fresh_input_excludes_cache_reads_and_writes() {
        let usage = TokenUsage {
            input_tokens: 100,
            cached_input_tokens: 60,
            cache_write_input_tokens: 15,
            ..TokenUsage::default()
        };

        assert_eq!(usage.fresh_input_tokens(), 25);
    }

    #[test]
    fn cache_hit_rate_excludes_input_without_cache_details() {
        let mut usage = UsageSummary::default();
        usage.record(TokenUsage {
            input_tokens: 100,
            output_tokens: 10,
            cached_input_tokens: 50,
            cache_write_input_tokens: 0,
            cache_details_reported: true,
        });
        usage.record(TokenUsage {
            input_tokens: 900,
            output_tokens: 10,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            cache_details_reported: false,
        });

        assert_eq!(usage.cache_reported_input_tokens, 100);
        assert_eq!(usage.cache_hit_percent(), 50);
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
