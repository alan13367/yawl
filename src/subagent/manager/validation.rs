//! Subagent input validation and ID/index resolution.

use std::collections::HashSet;
use std::time::Duration;

use crate::config::Config;
use crate::subagent::types::{MAX_PROMPT_CHARS, MAX_TRACKED_SUBAGENTS};

use super::State;

const MAX_WAIT_SECS: u64 = 300;

pub(super) fn validate_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("name must not be empty".into());
    }
    if name.chars().count() > 160 {
        return Err("name must be no longer than 160 characters".into());
    }
    if name.chars().any(char::is_control) {
        return Err("name must not contain control characters".into());
    }
    Ok(name.to_string())
}

pub(super) fn validate_message(message: &str, label: &str) -> Result<String, String> {
    if message.trim().is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if message.chars().count() > MAX_PROMPT_CHARS {
        return Err(format!(
            "{label} must be no longer than {MAX_PROMPT_CHARS} characters"
        ));
    }
    Ok(message.to_string())
}

pub(super) fn resolve_model(
    config: &Config,
    parent_model: &str,
    preset_model: Option<&str>,
) -> Result<String, String> {
    if let Some(model) = preset_model.map(str::trim) {
        if model.is_empty() {
            return Err("model must not be empty".into());
        }
        return validate_resolved_model(config, model.to_string());
    }
    if config.subagent_model != "inherit" {
        return validate_resolved_model(config, config.subagent_model.clone());
    }
    validate_resolved_model(config, parent_model.to_string())
}

pub(super) fn validate_resolved_model(config: &Config, model: String) -> Result<String, String> {
    crate::provider::resolve(&model, config)
        .map(|_| model.clone())
        .map_err(|error| format!("model '{model}' is not usable: {error}"))
}

pub(super) fn validate_id_list(ids: &[String]) -> Result<(), String> {
    if ids.is_empty() || ids.len() > MAX_TRACKED_SUBAGENTS {
        return Err(format!(
            "ids must contain 1 through {MAX_TRACKED_SUBAGENTS} values"
        ));
    }
    let mut unique = HashSet::with_capacity(ids.len());
    if ids.iter().any(|id| !unique.insert(id)) {
        return Err("ids must not contain duplicates".into());
    }
    Ok(())
}

pub(super) fn find_index(state: &State, id: &str) -> Result<usize, String> {
    state
        .entries
        .iter()
        .position(|entry| entry.snapshot.id.as_str() == id)
        .ok_or_else(|| unknown_ids(state, &[id.to_string()]))
}

pub(super) fn resolve_indexes(state: &State, ids: &[String]) -> Result<Vec<usize>, String> {
    let mut indexes = Vec::with_capacity(ids.len());
    let mut unknown = Vec::new();
    for id in ids {
        match state
            .entries
            .iter()
            .position(|entry| entry.snapshot.id.as_str() == id)
        {
            Some(index) => indexes.push(index),
            None => unknown.push(id.clone()),
        }
    }
    if unknown.is_empty() {
        Ok(indexes)
    } else {
        Err(unknown_ids(state, &unknown))
    }
}

pub(super) fn unknown_ids(state: &State, unknown: &[String]) -> String {
    let known = state
        .entries
        .iter()
        .map(|entry| entry.snapshot.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "unknown subagent ID(s): {}; known IDs: {}",
        unknown.join(", "),
        if known.is_empty() { "(none)" } else { &known }
    )
}

pub(super) fn wait_timeout(timeout_secs: Option<u64>) -> Result<Option<Duration>, String> {
    let Some(timeout_secs) = timeout_secs else {
        return Ok(None);
    };
    if !(1..=MAX_WAIT_SECS).contains(&timeout_secs) {
        return Err(format!(
            "timeout_secs must be between 1 and {MAX_WAIT_SECS}"
        ));
    }
    Ok(Some(Duration::from_secs(timeout_secs)))
}
