//! Anthropic Messages API with SSE streaming.

use std::io::BufReader;

use serde_json::{Value, json};

use super::{Event, Provider, Request, Role, SseReader, ToolCall, error_body, http_agent};
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
        let mut blocks: Vec<(u64, Block)> = Vec::new();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;

        for sse in SseReader::new(reader) {
            let sse = sse?;
            if sse.data.is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(&sse.data)?;
            let kind = sse.event.as_str();
            match kind {
                "message_start" => {
                    input_tokens = v["message"]["usage"]["input_tokens"].as_u64().unwrap_or(0);
                    // Prompt-cache reads/writes count toward context size.
                    input_tokens += v["message"]["usage"]["cache_read_input_tokens"]
                        .as_u64()
                        .unwrap_or(0);
                    input_tokens += v["message"]["usage"]["cache_creation_input_tokens"]
                        .as_u64()
                        .unwrap_or(0);
                }
                "content_block_start" => {
                    let index = v["index"].as_u64().unwrap_or(0);
                    let block = &v["content_block"];
                    let state = match block["type"].as_str() {
                        Some("tool_use") => Block::ToolUse {
                            id: block["id"].as_str().unwrap_or("").to_string(),
                            name: block["name"].as_str().unwrap_or("").to_string(),
                            args: String::new(),
                        },
                        _ => Block::Text,
                    };
                    blocks.push((index, state));
                }
                "content_block_delta" => {
                    let index = v["index"].as_u64().unwrap_or(0);
                    let delta = &v["delta"];
                    match delta["type"].as_str() {
                        Some("text_delta") => {
                            if let Some(t) = delta["text"].as_str() {
                                on_event(Event::TextDelta(t.to_string()));
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(part) = delta["partial_json"].as_str()
                                && let Some((_, Block::ToolUse { args, .. })) =
                                    blocks.iter_mut().find(|(i, _)| *i == index)
                            {
                                args.push_str(part);
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let index = v["index"].as_u64().unwrap_or(0);
                    if let Some(pos) = blocks.iter().position(|(i, _)| *i == index)
                        && let (_, Block::ToolUse { id, name, args }) = blocks.remove(pos)
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
                    if let Some(out) = v["usage"]["output_tokens"].as_u64() {
                        output_tokens = out;
                    }
                }
                "message_stop" => {
                    on_event(Event::Usage {
                        input_tokens,
                        output_tokens,
                    });
                    on_event(Event::Done);
                    return Ok(());
                }
                "error" => {
                    let etype = v["error"]["type"].as_str().unwrap_or("error");
                    let msg = v["error"]["message"].as_str().unwrap_or("unknown");
                    // Streaming errors do not carry an HTTP status. Map the
                    // retryable API error types back to their usual status.
                    let retry_status = match etype {
                        "rate_limit_error" => Some(429),
                        "api_error" => Some(500),
                        "overloaded_error" => Some(529),
                        _ => None,
                    };
                    if let Some(status) = retry_status {
                        return Err(Error::Http {
                            status,
                            body: msg.to_string(),
                        });
                    }
                    return Err(Error::Protocol(format!("{etype}: {msg}")));
                }
                _ => {} // ping etc.
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
}
