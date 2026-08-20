//! OpenAI Codex subscription provider using ChatGPT OAuth and the Responses
//! SSE endpoint. The OAuth and wire behavior follow OpenAI's Codex CLI flow.

use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    Event, Provider, ReasoningKind, Request, Role, SseEvent, SseReader, ToolCall, error_body,
    http_agent,
};
use crate::config::Config;
use crate::error::Error;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const DEVICE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const REFRESH_MARGIN_MS: u64 = 5 * 60 * 1000;

pub struct Codex {
    agent: ureq::Agent,
    access_token: String,
    account_id: String,
    reasoning_effort: Option<String>,
}

impl Codex {
    pub fn from_config(config: &Config) -> Result<Self, Error> {
        let credential = load_and_refresh_credential(config)?;
        Ok(Self {
            agent: http_agent(),
            access_token: credential.access,
            account_id: credential.account_id,
            reasoning_effort: config.reasoning_effort.clone(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodexCredential {
    #[serde(default = "oauth_type", rename = "type")]
    credential_type: String,
    access: String,
    refresh: String,
    expires: u64,
    #[serde(rename = "accountId", alias = "account_id")]
    account_id: String,
}

fn oauth_type() -> String {
    "oauth".into()
}

#[derive(Deserialize)]
struct DeviceStartResponse {
    device_auth_id: String,
    user_code: String,
    interval: Value,
}

struct DeviceAuth {
    id: String,
    user_code: String,
    interval: Duration,
}

#[derive(Deserialize)]
struct DeviceTokenResponse {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

/// Logs into a ChatGPT Plus or Pro subscription with OpenAI's device-code
/// flow and stores refreshable credentials in `~/.yawl/auth.json`.
///
/// # Errors
///
/// Returns an error for network failures, rejected login, malformed OAuth
/// responses, cancellation, or credential persistence failures.
pub fn login(config: &Config) -> Result<(), Error> {
    crate::set_interrupted(false);
    let agent = oauth_agent();
    let device = start_device_auth(&agent)?;
    println!(
        "\nOpen {DEVICE_VERIFICATION_URI} and enter this code:\n\n    {}\n\nWaiting for authorization...",
        device.user_code
    );
    let token = poll_device_auth(&agent, &device)?;
    let credential = exchange_code(&agent, &token.authorization_code, &token.code_verifier)?;
    save_credential(config, &credential)?;
    println!(
        "OpenAI Codex login saved to {}.",
        auth_path(config).display()
    );
    Ok(())
}

fn oauth_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into()
}

fn start_device_auth(agent: &ureq::Agent) -> Result<DeviceAuth, Error> {
    let body = json!({"client_id": CLIENT_ID}).to_string();
    let mut response = agent
        .post(DEVICE_USER_CODE_URL)
        .header("content-type", "application/json")
        .send(body)?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .with_config()
        .limit(256 * 1024)
        .read_to_string()?;
    if status != 200 {
        return Err(Error::Http { status, body: text });
    }
    let parsed: DeviceStartResponse = serde_json::from_str(&text)?;
    let interval_seconds = match parsed.interval {
        Value::Number(value) => value.as_f64(),
        Value::String(value) => value.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|value| value.is_finite() && *value >= 0.0)
    .ok_or_else(|| Error::Protocol("invalid Codex device-code polling interval".into()))?;
    Ok(DeviceAuth {
        id: parsed.device_auth_id,
        user_code: parsed.user_code,
        interval: Duration::from_millis((interval_seconds * 1000.0).max(1000.0) as u64),
    })
}

fn poll_device_auth(
    agent: &ureq::Agent,
    device: &DeviceAuth,
) -> Result<DeviceTokenResponse, Error> {
    let deadline = std::time::Instant::now() + DEVICE_TIMEOUT;
    let mut interval = device.interval;
    while std::time::Instant::now() < deadline {
        if crate::interrupted() {
            return Err(Error::Interrupted);
        }
        let body = json!({
            "device_auth_id": device.id,
            "user_code": device.user_code,
        })
        .to_string();
        let mut response = agent
            .post(DEVICE_TOKEN_URL)
            .header("content-type", "application/json")
            .send(body)?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .with_config()
            .limit(256 * 1024)
            .read_to_string()?;
        if status == 200 {
            return serde_json::from_str(&text).map_err(Error::from);
        }
        if !matches!(status, 403 | 404) {
            let error_code = serde_json::from_str::<Value>(&text).ok().and_then(|value| {
                value["error"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| value["error"]["code"].as_str().map(str::to_string))
            });
            if error_code.as_deref() == Some("slow_down") {
                interval += Duration::from_secs(5);
            } else if error_code.as_deref() != Some("deviceauth_authorization_pending") {
                return Err(Error::Http { status, body: text });
            }
        }
        interruptible_sleep(interval)?;
    }
    Err(Error::Config("OpenAI Codex device login timed out".into()))
}

fn interruptible_sleep(duration: Duration) -> Result<(), Error> {
    let deadline = std::time::Instant::now() + duration;
    while std::time::Instant::now() < deadline {
        if crate::interrupted() {
            return Err(Error::Interrupted);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        std::thread::sleep(remaining.min(Duration::from_millis(100)));
    }
    Ok(())
}

fn exchange_code(
    agent: &ureq::Agent,
    code: &str,
    verifier: &str,
) -> Result<CodexCredential, Error> {
    let form = form_body(&[
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", DEVICE_REDIRECT_URI),
    ]);
    token_request(agent, &form, "exchange")
}

fn refresh_credential(refresh: &str) -> Result<CodexCredential, Error> {
    let agent = oauth_agent();
    let form = form_body(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh),
        ("client_id", CLIENT_ID),
    ]);
    token_request(&agent, &form, "refresh")
}

fn token_request(
    agent: &ureq::Agent,
    form: &str,
    operation: &str,
) -> Result<CodexCredential, Error> {
    let mut response = agent
        .post(TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .send(form)?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .with_config()
        .limit(512 * 1024)
        .read_to_string()?;
    if status != 200 {
        return Err(Error::Http { status, body: text });
    }
    let token: TokenResponse = serde_json::from_str(&text).map_err(|error| {
        Error::Protocol(format!("invalid Codex token {operation} response: {error}"))
    })?;
    let account_id = account_id_from_jwt(&token.access_token)?;
    Ok(CodexCredential {
        credential_type: oauth_type(),
        access: token.access_token,
        refresh: token.refresh_token,
        expires: now_millis().saturating_add(token.expires_in.saturating_mul(1000)),
        account_id,
    })
}

fn form_body(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(name, value)| format!("{}={}", percent_encode(name), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn account_id_from_jwt(token: &str) -> Result<String, Error> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| Error::Protocol("invalid Codex access token".into()))?;
    let decoded = decode_base64_url(payload)?;
    let json: Value = serde_json::from_slice(&decoded)?;
    json[JWT_CLAIM_PATH]["chatgpt_account_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Error::Protocol("Codex access token has no account ID".into()))
}

fn decode_base64_url(value: &str) -> Result<Vec<u8>, Error> {
    let mut output = Vec::with_capacity(value.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for byte in value.bytes().filter(|byte| *byte != b'=') {
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => {
                return Err(Error::Protocol(
                    "invalid base64 in Codex access token".into(),
                ));
            }
        };
        buffer = (buffer << 6) | u32::from(digit);
        bits += 6;
        while bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1u32 << bits).saturating_sub(1);
        }
    }
    Ok(output)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn auth_path(config: &Config) -> std::path::PathBuf {
    config.home_dir.join("auth.json")
}

fn load_and_refresh_credential(config: &Config) -> Result<CodexCredential, Error> {
    let mut credential = load_credential(config)?.ok_or_else(|| {
        Error::Config("OpenAI Codex is not logged in; run 'yawl --login openai-codex'".into())
    })?;
    if now_millis().saturating_add(REFRESH_MARGIN_MS) >= credential.expires {
        credential = refresh_credential(&credential.refresh)?;
        save_credential(config, &credential)?;
    }
    Ok(credential)
}

fn load_credential(config: &Config) -> Result<Option<CodexCredential>, Error> {
    let path = auth_path(config);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::Config(format!("{}: {error}", path.display()))),
    };
    let root: Value = serde_json::from_str(&text)
        .map_err(|error| Error::Config(format!("{}: {error}", path.display())))?;
    match root.get("openai-codex") {
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|error| Error::Config(format!("{}: {error}", path.display()))),
        None => Ok(None),
    }
}

fn save_credential(config: &Config, credential: &CodexCredential) -> Result<(), Error> {
    let path = auth_path(config);
    let mut root = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .map_err(|error| Error::Config(format!("{}: {error}", path.display())))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(Error::Config(format!("{}: {error}", path.display()))),
    };
    let Value::Object(object) = &mut root else {
        return Err(Error::Config(format!(
            "{}: top-level JSON value must be an object",
            path.display()
        )));
    };
    object.insert("openai-codex".into(), serde_json::to_value(credential)?);
    write_private_json(&path, &root)
}

fn write_private_json(path: &Path, value: &Value) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok::<(), Error>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn build_input(messages: &[super::Message]) -> Vec<Value> {
    let mut input = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        match message.role {
            Role::User => input.push(json!({
                "role": "user",
                "content": [{"type": "input_text", "text": message.content}],
            })),
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
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": message.content,
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
        "input": build_input(request.messages),
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
    body
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
            "response.output_item.done" => self.output_item(&value["item"], on_event),
            "response.completed" | "response.done" | "response.incomplete" => {
                let terminal = &value["response"];
                if let Some(output) = terminal["output"].as_array() {
                    for item in output {
                        self.output_item(item, on_event);
                    }
                }
                let usage = &terminal["usage"];
                on_event(Event::Usage {
                    input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
                    output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
                });
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
        let body = build_body(req, self.reasoning_effort.as_deref()).to_string();
        let mut response = self
            .agent
            .post(CODEX_RESPONSES_URL)
            .header("authorization", format!("Bearer {}", self.access_token))
            .header("chatgpt-account-id", &self.account_id)
            .header("originator", "yawl")
            .header("user-agent", format!("yawl/{}", env!("CARGO_PKG_VERSION")))
            .header("openai-beta", "responses=experimental")
            .header("accept", "text/event-stream")
            .header("content-type", "application/json")
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
    use crate::provider::Message;
    use std::io::Cursor;

    #[test]
    fn decodes_url_safe_jwt_account_id() {
        // {"https://api.openai.com/auth":{"chatgpt_account_id":"acc_test"}}
        let payload = "eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjX3Rlc3QifX0";
        assert_eq!(
            account_id_from_jwt(&format!("header.{payload}.signature")).unwrap(),
            "acc_test"
        );
    }

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
        let input = build_input(&messages);
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
    }

    #[test]
    fn request_includes_selected_reasoning_effort() {
        let request = Request {
            model: "gpt-5.6-sol",
            system: "test",
            messages: &[],
            tools: &[],
            max_tokens: 1024,
        };
        let body = build_body(&request, Some("max"));
        assert_eq!(body["reasoning"]["effort"], "max");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(
            build_body(&request, Some("minimal"))["reasoning"]["effort"],
            "low"
        );
        assert!(build_body(&request, None).get("reasoning").is_none());
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
    fn percent_encodes_form_values() {
        assert_eq!(percent_encode("a+b/c="), "a%2Bb%2Fc%3D");
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
                "data: {{\"type\":\"response.output_item.done\",\"item\":{function_call}}}\n\n",
                "data: {{\"type\":\"response.output_item.done\",\"item\":{reasoning}}}\n\n",
                "data: {{\"type\":\"response.completed\",\"response\":{{\"output\":[{function_call},{reasoning}],\"usage\":{{\"input_tokens\":13,\"output_tokens\":6}}}}}}\n\n",
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
        assert!(matches!(
            &events[3],
            Event::ToolCall(call)
                if call.id == "call_1|fc_1"
                    && call.name == "shell"
                    && call.arguments == r#"{"command":"pwd"}"#
        ));
        assert!(matches!(
            events[4],
            Event::Usage {
                input_tokens: 13,
                output_tokens: 6
            }
        ));
        assert!(matches!(
            &events[5],
            Event::ProviderData(item)
                if item["id"] == "rs_1" && item["encrypted_content"] == "secret"
        ));
        assert!(matches!(events[6], Event::Done));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::ToolCall(_)))
                .count(),
            1
        );
    }
}
