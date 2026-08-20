//! OpenAI Codex subscription provider using ChatGPT OAuth and the Responses
//! SSE endpoint. The OAuth and wire behavior follow OpenAI's Codex CLI flow.

mod auth;
mod responses;

pub use auth::login;

use super::http_agent;
use crate::config::Config;
use crate::error::Error;

pub struct Codex {
    agent: ureq::Agent,
    access_token: String,
    account_id: String,
    reasoning_effort: Option<String>,
}

impl Codex {
    pub fn from_config(config: &Config) -> Result<Self, Error> {
        let credential = auth::load_and_refresh_credential(config)?;
        Ok(Self {
            agent: http_agent(),
            access_token: credential.access,
            account_id: credential.account_id,
            reasoning_effort: config.reasoning_effort.clone(),
        })
    }
}
