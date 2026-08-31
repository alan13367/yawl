use std::collections::{HashMap, HashSet};
use std::io::BufReader;

use serde_json::{Value, json};

use super::Codex;
use crate::error::Error;
use crate::provider::types::clamp_openai_prompt_cache_key;
use crate::provider::{
    Event, Message, Provider, ReasoningKind, Request, Role, SseEvent, SseReader, TokenUsage,
    ToolCall, error_body,
};

const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

fn codex_image_block(image: &crate::provider::ImageContent) -> Value {
    json!({
        "type": "input_image",
        "image_url": format!("data:{};base64,{}", image.media_type, image.data),
        "detail": "auto",
    })
}

fn build_input(messages: &[Message], supports_images: bool) -> Vec<Value> {
    let mut input = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        match message.role {
            Role::User => {
                let mut content = vec![json!({"type": "input_text", "text": message.content})];
                if supports_images {
                    content.extend(message.images.iter().map(codex_image_block));
                }
                input.push(json!({"role": "user", "content": content}));
            }
            Role::Assistant => {
                input.extend(message.provider_data.iter().cloned());
                if !message.content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "id": format!("msg_yawl_{message_index}"),
                        "role": "assistant",
                        "status": "completed",
                        "content": [{
                            "type": "output_text",
                            "text": message.content,
                            "annotations": [],
                        }],
                    }));
                }
                for (call_index, call) in message.tool_calls.iter().enumerate() {
                    let (call_id, item_id) = call
                        .id
                        .split_once('|')
                        .map_or((call.id.as_str(), None), |(call_id, item_id)| {
                            (call_id, Some(item_id))
                        });
                    input.push(json!({
                        "type": "function_call",
                        "id": item_id.map(str::to_string).unwrap_or_else(|| format!("fc_yawl_{message_index}_{call_index}")),
                        "call_id": call_id,
                        "name": call.name,
                        "arguments": call.arguments,
                        "status": "completed",
                    }));
                }
            }
            Role::Tool => {
                let call_id = message
                    .tool_call_id
                    .as_deref()
                    .unwrap_or("")
                    .split_once('|')
                    .map_or_else(
                        || message.tool_call_id.as_deref().unwrap_or(""),
                        |(call_id, _)| call_id,
                    );
                let output = if supports_images && !message.images.is_empty() {
                    let mut output = vec![json!({"type": "input_text", "text": message.content})];
                    output.extend(message.images.iter().map(codex_image_block));
                    Value::Array(output)
                } else {
                    Value::String(message.content.clone())
                };
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                }));
            }
        }
    }
    input
}

fn build_body(request: &Request<'_>, reasoning_effort: Option<&str>) -> Value {
    let mut body = json!({
        "model": request.model,
        "store": false,
        "stream": true,
        "instructions": if request.system.is_empty() { "You are a helpful assistant." } else { request.system },
        "input": build_input(request.messages, request.supports_images),
        "text": {"verbosity": "low"},
        "include": ["reasoning.encrypted_content"],
        "tool_choice": "auto",
        "parallel_tool_calls": true,
    });
    if let Some(effort) = reasoning_effort {
        // Codex exposes a Minimal UI level but currently maps it to the
        // service's lowest accepted wire value.
        let effort = if effort == "minimal" { "low" } else { effort };
        body["reasoning"] = json!({"effort": effort, "summary": "auto"});
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                        "strict": null,
                    })
                })
                .collect(),
        );
    }
    if request.prompt_cache_control
        && let Some(key) = clamp_openai_prompt_cache_key(request.prompt_cache_key)
    {
        body["prompt_cache_key"] = json!(key);
    }
    body
}

fn cache_affinity_headers(cache_key: &str) -> [(&'static str, &str); 2] {
    [
        ("session-id", cache_key),
        ("x-client-request-id", cache_key),
    ]
}

#[derive(Default)]
struct Decoder {
    emitted_calls: HashSet<String>,
    reasoning_items: HashMap<String, Value>,
    emitted_summary: bool,
}

impl Decoder {
    fn decode(&mut self, event: SseEvent, on_event: &mut dyn FnMut(Event)) -> Result<bool, Error> {
        if event.data.is_empty() || event.data == "[DONE]" {
            return Ok(false);
        }
        let value: Value = serde_json::from_str(&event.data)?;
        match value["type"].as_str().unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(delta) = value["delta"].as_str()
                    && !delta.is_empty()
                {
                    on_event(Event::TextDelta(delta.to_string()));
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = value["delta"].as_str()
                    && !delta.is_empty()
                {
                    self.emitted_summary = true;
                    on_event(Event::ReasoningDelta {
                        kind: ReasoningKind::Summary,
                        text: delta.to_string(),
                    });
                }
            }
            "response.reasoning_summary_part.done" if self.emitted_summary => {
                on_event(Event::ReasoningDelta {
                    kind: ReasoningKind::Summary,
                    text: "\n\n".into(),
                });
            }
            "response.output_item.added" if value["item"]["type"] == "function_call" => {
                if let Some(name) = value["item"]["name"].as_str()
                    && !name.is_empty()
                {
                    on_event(Event::ToolCallName(name.to_string()));
                }
            }
            "response.output_item.done" => self.output_item(&value["item"], on_event),
            "response.completed" | "response.done" | "response.incomplete" => {
                let terminal = &value["response"];
                if let Some(output) = terminal["output"].as_array() {
                    for item in output {
                        self.output_item(item, on_event);
                    }
                }
                let usage = &terminal["usage"];
                let input_details = &usage["input_tokens_details"];
                on_event(Event::Usage(TokenUsage {
                    input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
                    output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
                    cached_input_tokens: input_details["cached_tokens"].as_u64().unwrap_or(0),
                    cache_write_input_tokens: input_details["cache_write_tokens"]
                        .as_u64()
                        .unwrap_or(0),
                    cache_details_reported: input_details.is_object(),
                }));
                let mut reasoning = std::mem::take(&mut self.reasoning_items)
                    .into_iter()
                    .collect::<Vec<_>>();
                reasoning.sort_by(|left, right| left.0.cmp(&right.0));
                for (_, item) in reasoning {
                    on_event(Event::ProviderData(item));
                }
                on_event(Event::Done);
                return Ok(true);
            }
            "response.failed" => {
                let error = &value["response"]["error"];
                return Err(Error::Protocol(
                    error["message"]
                        .as_str()
                        .or_else(|| error["code"].as_str())
                        .unwrap_or("Codex response failed")
                        .to_string(),
                ));
            }
            "error" => {
                let error = &value["error"];
                return Err(Error::Protocol(
                    error["message"]
                        .as_str()
                        .or_else(|| value["message"].as_str())
                        .unwrap_or("Codex stream error")
                        .to_string(),
                ));
            }
            _ => {}
        }
        Ok(false)
    }

    fn output_item(&mut self, item: &Value, on_event: &mut dyn FnMut(Event)) {
        if item["type"] == "function_call" {
            emit_tool_call(item, &mut self.emitted_calls, on_event);
        } else if item["type"] == "reasoning"
            && let Some(id) = item["id"].as_str()
        {
            emit_reasoning_summary(item, &mut self.emitted_summary, on_event);
            self.reasoning_items.insert(id.to_string(), item.clone());
        }
    }
}

impl Provider for Codex {
    fn stream_once(&self, req: &Request<'_>, on_event: &mut dyn FnMut(Event)) -> Result<(), Error> {
        let cache_key = if req.prompt_cache_control {
            clamp_openai_prompt_cache_key(req.prompt_cache_key)
        } else {
            None
        };
        let body = build_body(req, self.reasoning_effort.as_deref()).to_string();
        let mut request = self
            .agent
            .post(CODEX_RESPONSES_URL)
            .header("authorization", format!("Bearer {}", self.access_token))
            .header("chatgpt-account-id", &self.account_id)
            .header("originator", "yawl")
            .header("user-agent", format!("yawl/{}", env!("CARGO_PKG_VERSION")))
            .header("openai-beta", "responses=experimental")
            .header("accept", "text/event-stream")
            .header("content-type", "application/json");
        if let Some(cache_key) = cache_key.as_deref() {
            for (name, value) in cache_affinity_headers(cache_key) {
                request = request.header(name, value);
            }
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
        let mut decoder = Decoder::default();
        for event in SseReader::new(reader) {
            if decoder.decode(event?, on_event)? {
                return Ok(());
            }
        }
        Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "Codex stream ended before completion",
        )))
    }
}

fn emit_reasoning_summary(item: &Value, emitted: &mut bool, on_event: &mut dyn FnMut(Event)) {
    if *emitted {
        return;
    }
    let Some(parts) = item["summary"].as_array() else {
        return;
    };
    let summary = parts
        .iter()
        .filter_map(|part| part["text"].as_str())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if !summary.is_empty() {
        on_event(Event::ReasoningDelta {
            kind: ReasoningKind::Summary,
            text: summary,
        });
        *emitted = true;
    }
}

fn emit_tool_call(item: &Value, emitted: &mut HashSet<String>, on_event: &mut dyn FnMut(Event)) {
    let call_id = item["call_id"].as_str().unwrap_or("");
    let item_id = item["id"].as_str().unwrap_or("");
    let unique_id = if item_id.is_empty() {
        call_id.to_string()
    } else {
        format!("{call_id}|{item_id}")
    };
    if call_id.is_empty() || !emitted.insert(unique_id.clone()) {
        return;
    }
    on_event(Event::ToolCall(ToolCall {
        id: unique_id,
        name: item["name"].as_str().unwrap_or("").to_string(),
        arguments: item["arguments"].as_str().unwrap_or("{}").to_string(),
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn request_replays_reasoning_and_tool_results() {
        let mut assistant = Message::assistant(
            String::new(),
            vec![ToolCall {
                id: "call_1|fc_1".into(),
                name: "shell".into(),
                arguments: "{\"command\":\"pwd\"}".into(),
            }],
        );
        assistant.provider_data = vec![json!({
            "type": "reasoning",
            "id": "rs_1",
            "encrypted_content": "encrypted"
        })];
        let messages = vec![
            Message::user("where am I?"),
            assistant,
            Message::tool_result("call_1|fc_1", "shell", "/tmp".into(), false),
        ];
        let input = build_input(&messages, false);
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
    }

    #[test]
    fn images_translate_for_user_and_function_output() {
        let image = crate::provider::ImageContent {
            media_type: "image/png".into(),
            data: "cG5n".into(),
        };
        let mut user = Message::user("inspect");
        user.images.push(image.clone());
        let tool = Message::tool_result_with_images(
            "call_1",
            "read_file",
            "read image".into(),
            vec![image],
            false,
        );
        let input = build_input(&[user, tool], true);

        assert_eq!(input[0]["content"][1]["type"], "input_image");
        assert_eq!(input[1]["output"][1]["type"], "input_image");
    }

    #[test]
    fn request_includes_selected_reasoning_effort() {
        let request = Request {
            model: "gpt-5.6-sol",
            system: "test",
            messages: &[],
            tools: &[],
            max_tokens: 1024,
            supports_images: false,
            prompt_cache_control: true,
            prompt_cache_key: Some("session-test"),
        };
        let body = build_body(&request, Some("max"));
        assert_eq!(body["reasoning"]["effort"], "max");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(
            build_body(&request, Some("minimal"))["reasoning"]["effort"],
            "low"
        );
        assert!(build_body(&request, None).get("reasoning").is_none());
        assert_eq!(body["prompt_cache_key"], "session-test");

        let long_key = "x".repeat(65);
        let long_request = Request {
            prompt_cache_key: Some(&long_key),
            ..request
        };
        assert_eq!(
            build_body(&long_request, None)["prompt_cache_key"],
            "x".repeat(64)
        );
        let uncached_request = Request {
            prompt_cache_control: false,
            ..long_request
        };
        assert!(
            build_body(&uncached_request, None)
                .get("prompt_cache_key")
                .is_none()
        );
    }

    #[test]
    fn cache_affinity_headers_match_the_clamped_body_key() {
        let long_key = "x".repeat(65);
        let key = clamp_openai_prompt_cache_key(Some(&long_key)).expect("cache key");
        let expected = "x".repeat(64);
        assert_eq!(
            cache_affinity_headers(&key),
            [
                ("session-id", expected.as_str()),
                ("x-client-request-id", expected.as_str()),
            ]
        );
    }

    #[test]
    fn completed_reasoning_item_emits_summary_once() {
        let item = json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{"type": "summary_text", "text": "Inspecting the request"}]
        });
        let mut emitted = false;
        let mut summaries = Vec::new();
        emit_reasoning_summary(&item, &mut emitted, &mut |event| {
            if let Event::ReasoningDelta { kind, text } = event {
                summaries.push((kind, text));
            }
        });
        emit_reasoning_summary(&item, &mut emitted, &mut |_| {});

        assert_eq!(
            summaries,
            [(ReasoningKind::Summary, "Inspecting the request".into())]
        );
        assert!(emitted);
    }

    #[test]
    fn decoder_handles_deltas_deduplicates_items_and_preserves_reasoning() {
        let function_call = r#"{"type":"function_call","id":"fc_1","call_id":"call_1","name":"shell","arguments":"{\"command\":\"pwd\"}"}"#;
        let reasoning = r#"{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"Inspecting"}],"encrypted_content":"secret"}"#;
        let fixture = format!(
            concat!(
                "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}}\n\n",
                "data: {{\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"think\"}}\n\n",
                "data: {{\"type\":\"response.reasoning_summary_part.done\"}}\n\n",
                "data: {{\"type\":\"response.output_item.added\",\"item\":{function_call}}}\n\n",
                "data: {{\"type\":\"response.output_item.done\",\"item\":{function_call}}}\n\n",
                "data: {{\"type\":\"response.output_item.done\",\"item\":{reasoning}}}\n\n",
                "data: {{\"type\":\"response.completed\",\"response\":{{\"output\":[{function_call},{reasoning}],\"usage\":{{\"input_tokens\":13,\"output_tokens\":6,\"input_tokens_details\":{{\"cached_tokens\":9,\"cache_write_tokens\":2}}}}}}}}\n\n",
            ),
            function_call = function_call,
            reasoning = reasoning,
        );
        let mut decoder = Decoder::default();
        let mut events = Vec::new();
        for sse in SseReader::new(Cursor::new(fixture)) {
            if decoder
                .decode(sse.expect("valid SSE fixture"), &mut |event| {
                    events.push(event)
                })
                .expect("valid Codex fixture")
            {
                break;
            }
        }

        assert!(matches!(&events[0], Event::TextDelta(text) if text == "hello"));
        assert!(matches!(
            &events[1],
            Event::ReasoningDelta {
                kind: ReasoningKind::Summary,
                text
            } if text == "think"
        ));
        assert!(matches!(
            &events[2],
            Event::ReasoningDelta {
                kind: ReasoningKind::Summary,
                text
            } if text == "\n\n"
        ));
        assert!(matches!(&events[3], Event::ToolCallName(name) if name == "shell"));
        assert!(matches!(
            &events[4],
            Event::ToolCall(call)
                if call.id == "call_1|fc_1"
                    && call.name == "shell"
                    && call.arguments == r#"{"command":"pwd"}"#
        ));
        assert!(matches!(
            events[5],
            Event::Usage(TokenUsage {
                input_tokens: 13,
                output_tokens: 6,
                cached_input_tokens: 9,
                cache_write_input_tokens: 2,
                cache_details_reported: true,
            })
        ));
        assert!(matches!(
            &events[6],
            Event::ProviderData(item)
                if item["id"] == "rs_1" && item["encrypted_content"] == "secret"
        ));
        assert!(matches!(events[7], Event::Done));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::ToolCall(_)))
                .count(),
            1
        );
    }
}
