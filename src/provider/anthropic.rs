//! Anthropic Messages API with SSE streaming.

use std::io::BufReader;

use serde_json::{Value, json};

use super::{
    Event, Provider, Request, Role, SseEvent, SseReader, ToolCall, error_body, http_agent,
};
use crate::error::Error;

pub struct Anthropic {
    agent: ureq::Agent,
    base_url: String,
    api_key: String,
}

impl Anthropic {
    pub fn new(base_url: String, api_key: String) -> Anthropic {
        Anthropic {
            agent: http_agent(),
            base_url,
            api_key,
        }
    }
}

/// Translate the provider-agnostic history to Anthropic's shape: tool results
/// become `tool_result` blocks inside user messages, with consecutive results
/// merged into one user message so they directly follow their `tool_use`.
fn build_messages(messages: &[super::Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        match m.role {
            Role::User => out.push(json!({
                "role": "user",
                "content": [{"type": "text", "text": m.content}],
            })),
            Role::Assistant => {
                let mut blocks: Vec<Value> = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(json!({"type": "text", "text": m.content}));
                }
                for tc in &m.tool_calls {
                    let input: Value =
                        serde_json::from_str(&tc.arguments).unwrap_or_else(|_| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": input,
                    }));
                }
                if blocks.is_empty() {
                    continue;
                }
                out.push(json!({"role": "assistant", "content": blocks}));
            }
            Role::Tool => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.as_deref().unwrap_or(""),
                    "content": m.content,
                    "is_error": m.is_error,
                });
                // Append to the previous user message if it is a tool-result
                // carrier; otherwise start a new one.
                let merged = out.last_mut().is_some_and(|last| {
                    last["role"] == "user" && last["content"][0]["type"] == "tool_result"
                });
                if merged {
                    if let Some(Value::Array(blocks)) = out.last_mut().map(|l| &mut l["content"]) {
                        blocks.push(block);
                    }
                } else {
                    out.push(json!({"role": "user", "content": [block]}));
                }
            }
        }
    }
    out
}

fn build_body(req: &Request<'_>) -> Value {
    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "stream": true,
        "messages": build_messages(req.messages),
    });
    if !req.system.is_empty() {
        body["system"] = json!(req.system);
    }
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();
        body["tools"] = json!(tools);
    }
    body
}

/// In-flight content block state while streaming.
enum Block {
    Text,
    ToolUse {
        id: String,
        name: String,
        args: String,
    },
}

#[derive(Default)]
struct Decoder {
    blocks: Vec<(u64, Block)>,
    input_tokens: u64,
    output_tokens: u64,
}

impl Decoder {
    fn decode(&mut self, sse: SseEvent, on_event: &mut dyn FnMut(Event)) -> Result<bool, Error> {
        if sse.data.is_empty() {
            return Ok(false);
        }
        let value: Value = serde_json::from_str(&sse.data)?;
        match sse.event.as_str() {
            "message_start" => {
                self.input_tokens = value["message"]["usage"]["input_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                // Prompt-cache reads/writes count toward context size.
                self.input_tokens += value["message"]["usage"]["cache_read_input_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                self.input_tokens += value["message"]["usage"]["cache_creation_input_tokens"]
                    .as_u64()
                    .unwrap_or(0);
            }
            "content_block_start" => {
                let index = value["index"].as_u64().unwrap_or(0);
                let block = &value["content_block"];
                let state = match block["type"].as_str() {
                    Some("tool_use") => Block::ToolUse {
                        id: block["id"].as_str().unwrap_or("").to_string(),
                        name: block["name"].as_str().unwrap_or("").to_string(),
                        args: String::new(),
                    },
                    _ => Block::Text,
                };
                self.blocks.push((index, state));
            }
            "content_block_delta" => {
                let index = value["index"].as_u64().unwrap_or(0);
                let delta = &value["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        if let Some(text) = delta["text"].as_str() {
                            on_event(Event::TextDelta(text.to_string()));
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(part) = delta["partial_json"].as_str()
                            && let Some((_, Block::ToolUse { args, .. })) =
                                self.blocks.iter_mut().find(|(i, _)| *i == index)
                        {
                            args.push_str(part);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = value["index"].as_u64().unwrap_or(0);
                if let Some(position) = self.blocks.iter().position(|(i, _)| *i == index)
                    && let (_, Block::ToolUse { id, name, args }) = self.blocks.remove(position)
                {
                    let arguments = if args.trim().is_empty() {
                        "{}".to_string()
                    } else {
                        args
                    };
                    on_event(Event::ToolCall(ToolCall {
                        id,
                        name,
                        arguments,
                    }));
                }
            }
            "message_delta" => {
                if let Some(output_tokens) = value["usage"]["output_tokens"].as_u64() {
                    self.output_tokens = output_tokens;
                }
            }
            "message_stop" => {
                on_event(Event::Usage {
                    input_tokens: self.input_tokens,
                    output_tokens: self.output_tokens,
                });
                on_event(Event::Done);
                return Ok(true);
            }
            "error" => {
                let error_type = value["error"]["type"].as_str().unwrap_or("error");
                let message = value["error"]["message"].as_str().unwrap_or("unknown");
                // Streaming errors do not carry an HTTP status. Map the
                // retryable API error types back to their usual status.
                let retry_status = match error_type {
                    "rate_limit_error" => Some(429),
                    "api_error" => Some(500),
                    "overloaded_error" => Some(529),
                    _ => None,
                };
                if let Some(status) = retry_status {
                    return Err(Error::Http {
                        status,
                        body: message.to_string(),
                    });
                }
                return Err(Error::Protocol(format!("{error_type}: {message}")));
            }
            _ => {} // ping etc.
        }
        Ok(false)
    }
}

impl Provider for Anthropic {
    fn stream_once(&self, req: &Request<'_>, on_event: &mut dyn FnMut(Event)) -> Result<(), Error> {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let body = build_body(req).to_string();
        let mut response = self
            .agent
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .send(body)?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(Error::Http {
                status,
                body: error_body(&mut response),
            });
        }

        let reader = BufReader::new(response.into_body().into_reader());
        let mut decoder = Decoder::default();
        for sse in SseReader::new(reader) {
            if decoder.decode(sse?, on_event)? {
                return Ok(());
            }
        }
        // Stream ended without message_stop: mid-stream disconnect, retryable.
        Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "stream ended before message_stop",
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;
    use std::io::Cursor;

    #[test]
    fn tool_results_merge_into_one_user_message() {
        let messages = vec![
            Message::user("hi"),
            Message::assistant(
                "".into(),
                vec![
                    ToolCall {
                        id: "a".into(),
                        name: "shell".into(),
                        arguments: "{\"command\":\"ls\"}".into(),
                    },
                    ToolCall {
                        id: "b".into(),
                        name: "read_file".into(),
                        arguments: "{}".into(),
                    },
                ],
            ),
            Message::tool_result("a", "shell", "out-a".into(), false),
            Message::tool_result("b", "read_file", "out-b".into(), true),
        ];
        let wire = build_messages(&messages);
        assert_eq!(wire.len(), 3);
        assert_eq!(wire[2]["role"], "user");
        assert_eq!(wire[2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(wire[2]["content"][1]["is_error"], true);
    }

    #[test]
    fn decoder_handles_text_fragmented_tools_cache_usage_and_completion() {
        let fixture = concat!(
            "event: message_start\n",
            "data: {\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":2,\"cache_creation_input_tokens\":3}}}\n\n",
            "event: content_block_start\n",
            "data: {\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"shell\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"pwd\\\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"index\":1}\n\n",
            "event: message_delta\n",
            "data: {\"usage\":{\"output_tokens\":4}}\n\n",
            "event: message_stop\n",
            "data: {}\n\n",
        );
        let mut decoder = Decoder::default();
        let mut events = Vec::new();
        for sse in SseReader::new(Cursor::new(fixture)) {
            if decoder
                .decode(sse.expect("valid SSE fixture"), &mut |event| {
                    events.push(event)
                })
                .expect("valid Anthropic fixture")
            {
                break;
            }
        }

        assert!(matches!(&events[0], Event::TextDelta(text) if text == "hello"));
        assert!(matches!(
            &events[1],
            Event::ToolCall(call)
                if call.id == "call_1"
                    && call.name == "shell"
                    && call.arguments == r#"{"command":"pwd"}"#
        ));
        assert!(matches!(
            events[2],
            Event::Usage {
                input_tokens: 15,
                output_tokens: 4
            }
        ));
        assert!(matches!(events[3], Event::Done));
    }
}
