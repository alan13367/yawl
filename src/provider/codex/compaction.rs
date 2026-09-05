//! Codex Responses remote compaction v2.

use std::io::BufReader;

use serde_json::{Value, json};

use super::Codex;
use super::responses::build_input;
use crate::error::Error;
use crate::provider::{CompactionOutput, Request, SseEvent, SseReader, TokenUsage, error_body};

const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const REMOTE_COMPACTION_FEATURE: &str = "remote_compaction_v2";
const RETAINED_USER_TOKEN_BUDGET: usize = 20_000;

pub(super) fn compact(codex: &Codex, request: &Request<'_>) -> Result<CompactionOutput, Error> {
    let input = build_input(request.messages, request.supports_images, request.model);
    let body = build_body(request, &input, codex.reasoning_effort.as_deref()).to_string();
    let mut http = codex
        .agent
        .post(CODEX_RESPONSES_URL)
        .header("authorization", format!("Bearer {}", codex.access_token))
        .header("chatgpt-account-id", &codex.account_id)
        .header("originator", "yawl")
        .header("user-agent", format!("yawl/{}", env!("CARGO_PKG_VERSION")))
        .header("openai-beta", "responses=experimental")
        .header("x-codex-beta-features", REMOTE_COMPACTION_FEATURE)
        .header("accept", "text/event-stream")
        .header("content-type", "application/json");
    if request.prompt_cache_control
        && let Some(key) =
            crate::provider::types::clamp_openai_prompt_cache_key(request.prompt_cache_key)
    {
        http = http
            .header("session-id", key.as_ref())
            .header("x-client-request-id", key.as_ref());
    }
    let mut response = http.send(body)?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(Error::Http {
            status,
            body: error_body(&mut response),
        });
    }

    let reader = BufReader::new(response.into_body().into_reader());
    let mut decoder = Decoder::default();
    let mut stream = SseReader::new(reader);
    while let Some(event) = stream.next() {
        if decoder.decode(event?)? {
            let result = decoder.finish(&input);
            if result.is_ok() {
                stream.finish();
            }
            return result;
        }
    }
    Err(Error::Io(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "Codex remote compaction stream ended before completion",
    )))
}

fn build_body(request: &Request<'_>, input: &[Value], reasoning_effort: Option<&str>) -> Value {
    let mut compact_input = input.to_vec();
    compact_input.push(json!({"type": "compaction_trigger"}));
    let mut body = json!({
        "model": request.model,
        "input": compact_input,
        "instructions": if request.system.is_empty() { "You are a helpful assistant." } else { request.system },
        "tools": request.tools.iter().map(|tool| json!({
            "type": "function",
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.input_schema,
            "strict": null,
        })).collect::<Vec<_>>(),
        "parallel_tool_calls": true,
        "tool_choice": "auto",
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "text": {"verbosity": "low"},
    });
    if let Some(effort) = reasoning_effort {
        let effort = if effort == "minimal" { "low" } else { effort };
        body["reasoning"] = json!({"effort": effort, "summary": "auto"});
    }
    if request.prompt_cache_control
        && let Some(key) =
            crate::provider::types::clamp_openai_prompt_cache_key(request.prompt_cache_key)
    {
        body["prompt_cache_key"] = json!(key);
    }
    body
}

#[derive(Default)]
struct Decoder {
    compaction: Option<Value>,
    usage: TokenUsage,
}

impl Decoder {
    fn decode(&mut self, event: SseEvent) -> Result<bool, Error> {
        if event.data.is_empty() || event.data == "[DONE]" {
            return Ok(false);
        }
        let value: Value = serde_json::from_str(&event.data)?;
        match value["type"].as_str().unwrap_or("") {
            "response.output_item.done" if value["item"]["type"] == "compaction" => {
                if self.compaction.replace(value["item"].clone()).is_some() {
                    return Err(Error::Protocol(
                        "Codex remote compaction returned more than one compaction item".into(),
                    ));
                }
            }
            "response.completed" => {
                self.usage = usage_from_response(&value["response"]);
                return Ok(true);
            }
            "response.failed" => {
                let error = &value["response"]["error"];
                return Err(Error::Protocol(
                    error["message"]
                        .as_str()
                        .or_else(|| error["code"].as_str())
                        .unwrap_or("Codex remote compaction failed")
                        .to_string(),
                ));
            }
            "error" => {
                let error = &value["error"];
                return Err(Error::Protocol(
                    error["message"]
                        .as_str()
                        .or_else(|| value["message"].as_str())
                        .unwrap_or("Codex remote compaction stream error")
                        .to_string(),
                ));
            }
            _ => {}
        }
        Ok(false)
    }

    fn finish(self, input: &[Value]) -> Result<CompactionOutput, Error> {
        let compaction = self.compaction.ok_or_else(|| {
            Error::Protocol("Codex remote compaction returned no compaction item".into())
        })?;
        Ok(CompactionOutput {
            replacement_history: replacement_history(input, compaction),
            usage: self.usage,
        })
    }
}

fn usage_from_response(response: &Value) -> TokenUsage {
    let usage = &response["usage"];
    let input_details = &usage["input_tokens_details"];
    TokenUsage {
        input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
        output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
        cached_input_tokens: input_details["cached_tokens"].as_u64().unwrap_or(0),
        cache_write_input_tokens: input_details["cache_write_tokens"]
            .as_u64()
            .or_else(|| input_details["cache_creation_tokens"].as_u64())
            .unwrap_or(0),
        cache_details_reported: input_details.is_object(),
    }
}

fn replacement_history(input: &[Value], compaction: Value) -> Vec<Value> {
    let users = input
        .iter()
        .filter(|item| item["type"] == "message" && item["role"] == "user")
        .filter(|item| {
            item["content"]
                .as_array()
                .is_some_and(|content| !content.is_empty())
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut history = retain_recent_users(&users, RETAINED_USER_TOKEN_BUDGET);
    history.push(compaction);
    history
}

fn retain_recent_users(messages: &[Value], token_budget: usize) -> Vec<Value> {
    let mut remaining = token_budget;
    let mut retained = Vec::new();
    for message in messages.iter().rev() {
        if remaining == 0 {
            break;
        }
        let tokens = approximate_tokens(message);
        if tokens <= remaining {
            retained.push(message.clone());
            remaining -= tokens;
        } else if let Some(message) = truncate_message(message, remaining.saturating_mul(4)) {
            retained.push(message);
            remaining = 0;
        }
    }
    retained.reverse();
    retained
}

fn approximate_tokens(message: &Value) -> usize {
    message_payload_len(message).div_ceil(4).max(1)
}

fn message_payload_len(message: &Value) -> usize {
    message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str().or_else(|| part["image_url"].as_str()))
        .map(str::len)
        .fold(0, usize::saturating_add)
}

fn truncate_message(message: &Value, mut remaining_chars: usize) -> Option<Value> {
    let mut truncated = message.clone();
    let content = truncated["content"].as_array_mut()?;
    content.retain_mut(|part| {
        let Some(payload) = part["text"].as_str().or_else(|| part["image_url"].as_str()) else {
            return true;
        };
        if remaining_chars == 0 {
            return false;
        }
        if payload.len() <= remaining_chars {
            remaining_chars -= payload.len();
            return true;
        }
        let Some(text) = part["text"].as_str() else {
            return false;
        };
        let end = text
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= remaining_chars)
            .last()
            .unwrap_or(0);
        if end == 0 {
            return false;
        }
        part["text"] = json!(&text[..end]);
        remaining_chars = 0;
        true
    });
    (!content.is_empty()).then_some(truncated)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::provider::{SseReader, ToolSpec};

    #[test]
    fn request_appends_trigger_and_mirrors_turn_settings() {
        let tools = [ToolSpec {
            name: "shell".into(),
            description: "run a command".into(),
            input_schema: json!({"type": "object"}),
        }];
        let request = Request {
            model: "gpt-5.6-sol",
            system: "system",
            messages: &[],
            tools: &tools,
            max_tokens: 1024,
            supports_images: false,
            prompt_cache_control: true,
            prompt_cache_key: Some("session-test"),
        };
        let body = build_body(&request, &[json!({"role": "user"})], Some("minimal"));

        assert_eq!(body["input"][1]["type"], "compaction_trigger");
        assert_eq!(body["tools"][0]["name"], "shell");
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["prompt_cache_key"], "session-test");
        assert_eq!(body["store"], false);
    }

    #[test]
    fn decoder_requires_one_compaction_item_and_reads_usage() {
        let fixture = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"opaque\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":20,\"input_tokens_details\":{\"cached_tokens\":60}}}}\n\n",
        );
        let mut decoder = Decoder::default();
        for event in SseReader::new(Cursor::new(fixture)) {
            if decoder
                .decode(event.expect("valid SSE"))
                .expect("valid event")
            {
                break;
            }
        }
        let input = [json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "remember this"}]
        })];
        let output = decoder.finish(&input).expect("compaction output");

        assert_eq!(output.replacement_history.len(), 2);
        assert_eq!(output.replacement_history[0]["role"], "user");
        assert_eq!(output.replacement_history[1]["type"], "compaction");
        assert_eq!(output.usage.input_tokens, 100);
        assert_eq!(output.usage.cached_input_tokens, 60);
    }

    #[test]
    fn retained_users_respect_the_twenty_thousand_token_budget() {
        let users = [
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"a".repeat(80_000)}]}),
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"tail"}]}),
        ];
        let retained = retain_recent_users(&users, RETAINED_USER_TOKEN_BUDGET);

        assert_eq!(retained.len(), 2);
        assert!(message_payload_len(&retained[0]) < 80_000);
        assert_eq!(retained[1]["content"][0]["text"], "tail");
        assert!(retained.iter().map(message_payload_len).sum::<usize>() <= 80_000);
    }

    #[test]
    fn retained_users_charge_images_and_never_truncate_their_urls() {
        let users = [json!({
            "type": "message",
            "role": "user",
            "content": [
                {"type": "input_text", "text": "caption"},
                {"type": "input_image", "image_url": format!("data:image/png;base64,{}", "a".repeat(100))},
            ],
        })];

        assert!(approximate_tokens(&users[0]) > 3);
        let retained = retain_recent_users(&users, 3);

        assert_eq!(retained.len(), 1);
        assert_eq!(
            retained[0]["content"],
            json!([{"type": "input_text", "text": "caption"}])
        );
    }
}
