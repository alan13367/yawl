use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::Config;
use crate::error::Error;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const DEVICE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const REFRESH_MARGIN_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct CodexCredential {
    #[serde(default = "oauth_type", rename = "type")]
    credential_type: String,
    pub(super) access: String,
    refresh: String,
    expires: u64,
    #[serde(rename = "accountId", alias = "account_id")]
    pub(super) account_id: String,
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

pub(super) fn load_and_refresh_credential(config: &Config) -> Result<CodexCredential, Error> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_url_safe_jwt_account_id() -> Result<(), Error> {
        // {"https://api.openai.com/auth":{"chatgpt_account_id":"acc_test"}}
        let payload = "eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjX3Rlc3QifX0";
        assert_eq!(
            account_id_from_jwt(&format!("header.{payload}.signature"))?,
            "acc_test"
        );
        Ok(())
    }

    #[test]
    fn percent_encodes_form_values() {
        assert_eq!(percent_encode("a+b/c="), "a%2Bb%2Fc%3D");
    }
}
