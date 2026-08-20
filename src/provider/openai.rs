//! OpenAI-compatible chat completions with SSE streaming. Also serves
//! Ollama, llama.cpp, and OpenRouter via a configurable `base_url`.

use std::io::BufReader;

use serde_json::{Value, json};

use super::{
    Event, Provider, ReasoningKind, Request, Role, SseEvent, SseReader, ToolCall, error_body,
    http_agent,
};
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
                let reasoning = m
                    .reasoning
                    .iter()
                    .filter(|reasoning| reasoning.kind == ReasoningKind::Full)
                    .map(|reasoning| reasoning.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !reasoning.is_empty() && compat.reasoning_content_on_assistant_messages() {
                    msg["reasoning_content"] = json!(reasoning);
                }
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

struct Decoder {
    pending: Vec<(u64, PendingCall)>,
    input_tokens: u64,
    output_tokens: u64,
    finished: bool,
    finish_reason_required: bool,
}

impl Decoder {
    fn new(compat: &OpenAiCompatibility) -> Self {
        Self {
            pending: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            finished: false,
            finish_reason_required: compat.finish_reason_in_stream(),
        }
    }

    fn decode(&mut self, sse: SseEvent, on_event: &mut dyn FnMut(Event)) -> Result<bool, Error> {
        if sse.data == "[DONE]" {
            self.complete(on_event);
            return Ok(true);
        }
        if sse.data.is_empty() {
            return Ok(false);
        }
        let value: Value = serde_json::from_str(&sse.data)?;
        if let Some(error) = value.get("error")
            && !error.is_null()
        {
            let message = error["message"].as_str().unwrap_or("unknown error");
            let kind = error["type"]
                .as_str()
                .or_else(|| error["code"].as_str())
                .unwrap_or("error");
            if kind.contains("rate_limit") {
                return Err(Error::Http {
                    status: 429,
                    body: message.to_string(),
                });
            }
            if kind.contains("server_error") || kind.contains("overloaded") {
                return Err(Error::Http {
                    status: 500,
                    body: message.to_string(),
                });
            }
            return Err(Error::Protocol(format!("server error: {message}")));
        }
        if let Some(usage) = value.get("usage")
            && usage.is_object()
        {
            if let Some(prompt_tokens) = usage["prompt_tokens"].as_u64() {
                self.input_tokens = prompt_tokens;
            }
            if let Some(completion_tokens) = usage["completion_tokens"].as_u64() {
                self.output_tokens = completion_tokens;
            }
        }
        let Some(choice) = value["choices"].get(0) else {
            return Ok(false);
        };
        let delta = &choice["delta"];
        if let Some(reasoning) = reasoning_delta(delta) {
            on_event(Event::ReasoningDelta {
                kind: ReasoningKind::Full,
                text: reasoning.to_string(),
            });
        }
        if let Some(text) = delta["content"].as_str()
            && !text.is_empty()
        {
            on_event(Event::TextDelta(text.to_string()));
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                let index = call["index"].as_u64().unwrap_or(0);
                let entry = self.pending_call(index);
                if let Some(id) = call["id"].as_str() {
                    entry.id.push_str(id);
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    entry.name.push_str(name);
                }
                if let Some(arguments) = call["function"]["arguments"].as_str() {
                    entry.arguments.push_str(arguments);
                }
            }
        }
        if !choice["finish_reason"].is_null() {
            self.finished = true;
        }
        Ok(false)
    }

    fn finish(mut self, on_event: &mut dyn FnMut(Event)) -> Result<(), Error> {
        // Some compatible servers close the stream without a [DONE] sentinel;
        // that is fine as long as a finish_reason arrived, or their configured
        // compatibility says not to require one.
        if self.finished || !self.finish_reason_required {
            self.complete(on_event);
            Ok(())
        } else {
            Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "stream ended before completion",
            )))
        }
    }

    fn pending_call(&mut self, index: u64) -> &mut PendingCall {
        let position = match self
            .pending
            .iter()
            .position(|(pending_index, _)| *pending_index == index)
        {
            Some(position) => position,
            None => {
                self.pending.push((index, PendingCall::default()));
                self.pending.len() - 1
            }
        };
        &mut self.pending[position].1
    }

    fn complete(&mut self, on_event: &mut dyn FnMut(Event)) {
        self.pending.sort_by_key(|(index, _)| *index);
        for (_, call) in self.pending.drain(..) {
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
        on_event(Event::Usage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
        });
        on_event(Event::Done);
    }
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
        let mut decoder = Decoder::new(&self.compat);
        for sse in SseReader::new(reader) {
            if decoder.decode(sse?, on_event)? {
                return Ok(());
            }
        }
        decoder.finish(on_event)
    }
}

fn reasoning_delta(delta: &Value) -> Option<&str> {
    ["reasoning_content", "reasoning", "reasoning_text"]
        .into_iter()
        .filter_map(|field| delta[field].as_str())
        .find(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;
    use std::io::Cursor;

    #[test]
    fn history_translates_to_openai_shape() {
        let mut assistant = Message::assistant(
            "using a tool".into(),
            vec![ToolCall {
                id: "call_1".into(),
                name: "shell".into(),
                arguments: "{\"command\":\"ls\"}".into(),
            }],
        );
        assistant.reasoning.push(super::super::Reasoning {
            kind: ReasoningKind::Full,
            content: "I should inspect the directory.".into(),
        });
        let messages = vec![
            Message::user("hi"),
            assistant,
            Message::tool_result("call_1", "shell", "file.txt".into(), false),
        ];
        let compat = OpenAiCompatibility {
            requires_reasoning_content_on_assistant_messages: Some(true),
            ..OpenAiCompatibility::default()
        };
        let wire = build_messages("sys", &messages, &compat);
        assert_eq!(wire.len(), 4);
        assert_eq!(wire[0]["role"], "system");
        assert_eq!(wire[2]["tool_calls"][0]["function"]["name"], "shell");
        assert_eq!(
            wire[2]["reasoning_content"],
            "I should inspect the directory."
        );
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
    fn reads_omlx_and_compatible_reasoning_deltas() {
        assert_eq!(
            reasoning_delta(&json!({"reasoning_content": "thinking"})),
            Some("thinking")
        );
        assert_eq!(
            reasoning_delta(&json!({"reasoning": "thinking"})),
            Some("thinking")
        );
        assert_eq!(
            reasoning_delta(&json!({"reasoning_text": "thinking"})),
            Some("thinking")
        );
        assert_eq!(reasoning_delta(&json!({"reasoning_content": ""})), None);
        assert_eq!(
            reasoning_delta(&json!({"reasoning_content": "", "reasoning": "fallback"})),
            Some("fallback")
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

    #[test]
    fn decoder_handles_reasoning_text_fragmented_tools_usage_and_done() {
        let fixture = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"command\\\":\\\"\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"pwd\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":5}}\n\n",
            "data: [DONE]\n\n",
        );
        let mut decoder = Decoder::new(&OpenAiCompatibility::default());
        let mut events = Vec::new();
        for sse in SseReader::new(Cursor::new(fixture)) {
            if decoder
                .decode(sse.expect("valid SSE fixture"), &mut |event| {
                    events.push(event)
                })
                .expect("valid OpenAI fixture")
            {
                break;
            }
        }

        assert!(matches!(
            &events[0],
            Event::ReasoningDelta {
                kind: ReasoningKind::Full,
                text
            } if text == "think"
        ));
        assert!(matches!(&events[1], Event::TextDelta(text) if text == "hello"));
        assert!(matches!(
            &events[2],
            Event::ToolCall(call)
                if call.id == "call_1"
                    && call.name == "shell"
                    && call.arguments == r#"{"command":"pwd"}"#
        ));
        assert!(matches!(
            events[3],
            Event::Usage {
                input_tokens: 11,
                output_tokens: 5
            }
        ));
        assert!(matches!(events[4], Event::Done));
    }
}
