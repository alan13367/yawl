//! OpenAI Codex subscription provider using ChatGPT OAuth and the Responses
//! SSE endpoint. The OAuth and wire behavior follow OpenAI's Codex CLI flow.

mod auth;
mod catalog;
mod compaction;
mod responses;

pub use auth::{CodexLoginStatus, credential_status, login};
pub(crate) use auth::{DeviceLoginPrompt, login_with_callback};
pub(crate) use catalog::{cached_models, refresh_catalog};

use std::path::PathBuf;
use std::sync::Mutex;

use super::http_agent;
use crate::config::Config;
use crate::error::Error;

type HttpResponse = ureq::http::Response<ureq::Body>;

pub struct Codex {
    agent: ureq::Agent,
    access_token: Mutex<String>,
    auth_path: PathBuf,
    account_id: String,
    reasoning_effort: Option<String>,
}

impl Codex {
    pub(super) fn with_reasoning_effort(mut self, effort: Option<&str>) -> Self {
        self.reasoning_effort = effort.map(str::to_string);
        self
    }

    pub fn from_config(config: &Config) -> Result<Self, Error> {
        let credential = auth::load_and_refresh_credential(config)?;
        Ok(Self {
            agent: http_agent(),
            access_token: Mutex::new(credential.access),
            auth_path: auth::auth_path(config),
            account_id: credential.account_id,
            reasoning_effort: config.reasoning_effort.clone(),
        })
    }

    fn access_token(&self) -> String {
        self.access_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Sends with the current access token. A 401 means ChatGPT rejected the
    /// token before its saved expiry, so refresh the login once and resend;
    /// the refreshed credential is saved for later requests too.
    fn send_authorized(
        &self,
        send: impl Fn(&str) -> Result<HttpResponse, Error>,
    ) -> Result<HttpResponse, Error> {
        let token = self.access_token();
        let response = send(&token)?;
        if response.status().as_u16() != 401 {
            return Ok(response);
        }
        let credential =
            auth::refresh_rejected_credential(&self.auth_path, &token).map_err(|error| {
                Error::Config(format!(
                    "OpenAI Codex rejected the saved login and refreshing it failed ({error}); \
                     run 'yawl --login openai-codex'"
                ))
            })?;
        *self
            .access_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = credential.access.clone();
        send(&credential.access)
    }
}
