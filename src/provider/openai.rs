//! OpenAI-compatible chat completions with SSE streaming. Also serves
//! Ollama, llama.cpp, and OpenRouter via a configurable `base_url`.

use std::io::BufReader;

use serde_json::{Value, json};

use super::{Event, Provider, Request, Role, SseReader, ToolCall, error_body, http_agent};
use crate::config::OpenAiCompatibility;
use crate::error::Error;

pub struct OpenAi {
    agent: ureq::Agent,
    base_url: String,
    api_key: String,
    auth_header: bool,
    headers: Vec<(String, String)>,
    compat: OpenAiCompatibility,
}

impl OpenAi {
    pub fn new(base_url: String, api_key: String) -> OpenAi {
        OpenAi::configured(
            base_url,
            api_key,
            true,
            Vec::new(),
            OpenAiCompatibility::default(),
        )
    }

    pub fn configured(
        base_url: String,
        api_key: String,
        auth_header: bool,
        headers: Vec<(String, String)>,
        compat: OpenAiCompatibility,
    ) -> OpenAi {
        OpenAi {
            agent: http_agent(),
            base_url,
            api_key,
            auth_header,
            headers,
            compat,
        }
    }
}

fn build_messages(
    system: &str,
    messages: &[super::Message],
    compat: &OpenAiCompatibility,
) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    if !system.is_empty() {
        out.push(json!({"role": "system", "content": system}));
    }
    for m in messages {
        match m.role {
            Role::User => out.push(json!({"role": "user", "content": m.content})),
            Role::Assistant => {
                let mut msg = json!({"role": "assistant", "content": m.content});
                if !m.tool_calls.is_empty() {
                    let calls: Vec<Value> = m
                        .tool_calls
                        .iter()
                        .map(|tc| {
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {"name": tc.name, "arguments": tc.arguments},
                            })
                        })
                        .collect();
                    msg["tool_calls"] = json!(calls);
                }
                out.push(msg);
            }
            Role::Tool => {
                let mut message = json!({
                    "role": "tool",
                    "tool_call_id": m.tool_call_id.as_deref().unwrap_or(""),
                    "content": m.content,
                });
                if compat.tool_result_name_required() {
                    message["name"] = json!(m.tool_name.as_deref().unwrap_or("tool"));
                }
                out.push(message);
            }
        }
    }
    out
}

fn build_body(req: &Request<'_>, compat: &OpenAiCompatibility) -> Value {
    let mut body = json!({
        "model": req.model,
        "stream": true,
        "messages": build_messages(req.system, req.messages, compat),
    });
    body[compat.max_tokens_field()] = json!(req.max_tokens);
    if compat.usage_in_stream() {
        body["stream_options"] = json!({"include_usage": true});
    }
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    },
                })
            })
            .collect();
        body["tools"] = json!(tools);
    }
    body
}

#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

impl Provider for OpenAi {
    fn stream_once(&self, req: &Request<'_>, on_event: &mut dyn FnMut(Event)) -> Result<(), Error> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = build_body(req, &self.compat).to_string();
        let mut request = self
            .agent
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream");
        let has_custom_authorization = self
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization"));
        if self.auth_header && !self.api_key.is_empty() && !has_custom_authorization {
            request = request.header("authorization", format!("Bearer {}", self.api_key));
        }
        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let mut response = request.send(body)?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(Error::Http {
                status,
                body: error_body(&mut response),
            });
        }

        let reader = BufReader::new(response.into_body().into_reader());
        // Indexed by the wire `index` field; calls stream as fragments.
        let mut pending: Vec<(u64, PendingCall)> = Vec::new();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut finished = false;

        let flush = |pending: &mut Vec<(u64, PendingCall)>, on_event: &mut dyn FnMut(Event)| {
            pending.sort_by_key(|(i, _)| *i);
            for (_, call) in pending.drain(..) {
                let arguments = if call.arguments.trim().is_empty() {
                    "{}".to_string()
                } else {
                    call.arguments
                };
                on_event(Event::ToolCall(ToolCall {
                    id: call.id,
                    name: call.name,
                    arguments,
                }));
            }
        };

        for sse in SseReader::new(reader) {
            let sse = sse?;
            if sse.data == "[DONE]" {
                flush(&mut pending, on_event);
                on_event(Event::Usage {
                    input_tokens,
                    output_tokens,
                });
                on_event(Event::Done);
                return Ok(());
            }
            if sse.data.is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(&sse.data)?;
            if let Some(err) = v.get("error")
                && !err.is_null()
            {
                let msg = err["message"].as_str().unwrap_or("unknown error");
                let kind = err["type"]
                    .as_str()
                    .or_else(|| err["code"].as_str())
                    .unwrap_or("error");
                if kind.contains("rate_limit") {
                    return Err(Error::Http {
                        status: 429,
                        body: msg.to_string(),
                    });
                }
                if kind.contains("server_error") || kind.contains("overloaded") {
                    return Err(Error::Http {
                        status: 500,
                        body: msg.to_string(),
                    });
                }
                return Err(Error::Protocol(format!("server error: {msg}")));
            }
            if let Some(usage) = v.get("usage")
                && usage.is_object()
            {
                if let Some(p) = usage["prompt_tokens"].as_u64() {
                    input_tokens = p;
                }
                if let Some(c) = usage["completion_tokens"].as_u64() {
                    output_tokens = c;
                }
            }
            let Some(choice) = v["choices"].get(0) else {
                continue;
            };
            let delta = &choice["delta"];
            if let Some(text) = delta["content"].as_str()
                && !text.is_empty()
            {
                on_event(Event::TextDelta(text.to_string()));
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                for c in calls {
                    let index = c["index"].as_u64().unwrap_or(0);
                    let entry = match pending.iter_mut().find(|(i, _)| *i == index) {
                        Some((_, e)) => e,
                        None => {
                            pending.push((index, PendingCall::default()));
                            &mut pending.last_mut().expect("just pushed").1
                        }
                    };
                    if let Some(id) = c["id"].as_str() {
                        entry.id.push_str(id);
                    }
                    if let Some(name) = c["function"]["name"].as_str() {
                        entry.name.push_str(name);
                    }
                    if let Some(args) = c["function"]["arguments"].as_str() {
                        entry.arguments.push_str(args);
                    }
                }
            }
            if !choice["finish_reason"].is_null() {
                finished = true;
            }
        }
        // Some compatible servers close the stream without a [DONE] sentinel;
        // that is fine as long as a finish_reason arrived.
        if finished || !self.compat.finish_reason_in_stream() {
            flush(&mut pending, on_event);
            on_event(Event::Usage {
                input_tokens,
                output_tokens,
            });
            on_event(Event::Done);
            return Ok(());
        }
        Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "stream ended before completion",
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;

    #[test]
    fn history_translates_to_openai_shape() {
        let messages = vec![
            Message::user("hi"),
            Message::assistant(
                "using a tool".into(),
                vec![ToolCall {
                    id: "call_1".into(),
                    name: "shell".into(),
                    arguments: "{\"command\":\"ls\"}".into(),
                }],
            ),
            Message::tool_result("call_1", "shell", "file.txt".into(), false),
        ];
        let wire = build_messages("sys", &messages, &OpenAiCompatibility::default());
        assert_eq!(wire.len(), 4);
        assert_eq!(wire[0]["role"], "system");
        assert_eq!(wire[2]["tool_calls"][0]["function"]["name"], "shell");
        assert_eq!(wire[3]["role"], "tool");
        assert_eq!(wire[3]["tool_call_id"], "call_1");

        let request = Request {
            model: "test",
            system: "sys",
            messages: &messages,
            tools: &[],
            max_tokens: 321,
        };
        assert_eq!(
            build_body(&request, &OpenAiCompatibility::default())["max_tokens"],
            321
        );
    }

    #[test]
    fn compatibility_can_omit_usage_and_name_tool_results() {
        let messages = vec![Message::tool_result("call_1", "shell", "ok".into(), false)];
        let compat = OpenAiCompatibility {
            supports_usage_in_streaming: Some(false),
            requires_tool_result_name: Some(true),
            max_tokens_field: Some("max_completion_tokens".into()),
            ..OpenAiCompatibility::default()
        };
        let request = Request {
            model: "test",
            system: "",
            messages: &messages,
            tools: &[],
            max_tokens: 123,
        };
        let body = build_body(&request, &compat);
        assert!(body.get("stream_options").is_none());
        assert_eq!(body["max_completion_tokens"], 123);
        assert_eq!(body["messages"][0]["name"], "shell");
    }
}
