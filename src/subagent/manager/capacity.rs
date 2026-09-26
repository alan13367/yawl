//! Subagent capacity: limits, admission, queueing, and pruning.

use std::collections::VecDeque;
use std::time::Duration;

use crate::agent::{Conversation, RunLimits};
use crate::config::Config;
use crate::subagent::names::generate_name;
use crate::subagent::presets::AgentPreset;
use crate::subagent::types::{
    MAX_QUEUE_MESSAGES, MAX_TRACKED_SUBAGENTS, QueuedSubagentMessage, RunOrigin, SubagentId,
    SubagentSnapshot, SubagentStatus,
};

use super::validation::{find_index, resolve_model, validate_message, validate_name};
use super::{Entry, State, SubagentManager, WorkItem};

impl SubagentManager {
    pub(crate) fn set_limit(&self, limit: usize) {
        self.lock().limit = limit.clamp(1, 16);
    }

    #[cfg(test)]
    pub(crate) fn spawn(
        &self,
        config: Config,
        parent_model: &str,
        name: Option<&str>,
        prompt: &str,
        preset: Option<&AgentPreset>,
    ) -> Result<SubagentId, String> {
        self.spawn_with_model(config, parent_model, name, prompt, preset, None)
    }

    pub(crate) fn spawn_with_model(
        &self,
        config: Config,
        parent_model: &str,
        name: Option<&str>,
        prompt: &str,
        preset: Option<&AgentPreset>,
        explicit_model: Option<&str>,
    ) -> Result<SubagentId, String> {
        let supplied_name = match name.map(str::trim) {
            Some(name) if !name.is_empty() => Some(validate_name(name)?),
            _ => None,
        };
        let prompt = validate_message(prompt, "prompt")?;
        let preset_model = preset
            .and_then(|preset| preset.model.as_deref())
            .map(str::trim)
            .filter(|model| !model.is_empty() && *model != "inherit");
        let model = resolve_model(&config, parent_model, preset_model, explicit_model)?;

        let (id, conversation, work) = {
            let mut state = self.lock();
            if state.shutting_down {
                return Err("subagent manager is shutting down".into());
            }
            prune_settled(&mut state);
            if state.entries.len() >= MAX_TRACKED_SUBAGENTS {
                return Err(format!(
                    "cannot track more than {MAX_TRACKED_SUBAGENTS} subagents; no settled entry is eligible for pruning"
                ));
            }
            if state.active >= state.limit {
                return Err(format!(
                    "subagent capacity is full ({}/{} running)",
                    state.active, state.limit
                ));
            }
            let id = SubagentId::new(state.next_id);
            state.next_id = state
                .next_id
                .checked_add(1)
                .ok_or_else(|| "subagent ID space is exhausted for this session".to_string())?;
            let name = match supplied_name {
                Some(name) => name,
                None => {
                    let existing = state
                        .entries
                        .iter()
                        .map(|entry| entry.snapshot.name.clone())
                        .collect::<Vec<_>>();
                    generate_name(&existing, state.next_id)
                }
            };
            let synthetic_session = format!("{}-{id}", state.session_id);
            let mut conversation =
                Conversation::memory(config.clone(), model.clone(), synthetic_session);
            // Match Codex's root/child cache-routing group while repeated
            // prefixes within each child remain independently reusable.
            conversation.set_prompt_cache_key(state.session_id.clone());
            conversation.set_run_limits(RunLimits {
                max_requests: config.subagent_request_budget as u64,
                timeout: (config.subagent_timeout_secs > 0)
                    .then(|| Duration::from_secs(config.subagent_timeout_secs)),
            });
            if let Some(preset) = preset {
                if let Some(tools) = &preset.tools {
                    conversation.set_tool_allowlist(tools.clone());
                }
                if let Some(fragment) = &preset.prompt {
                    conversation.set_role_fragment(fragment.clone());
                }
            }
            let cancellation = conversation.cancellation_token();
            let steers = conversation.steer_inbox();
            let work = WorkItem {
                message: prompt.clone(),
                origin: RunOrigin::Model,
                run_number: 1,
            };
            let snapshot = SubagentSnapshot::new(
                id.clone(),
                name,
                preset.map_or_else(|| "default".to_string(), |preset| preset.name.clone()),
                prompt,
                model.clone(),
                crate::model::context_window(&config, &model),
            );
            state.active = state.active.saturating_add(1);
            state.entries.push(Entry {
                snapshot,
                cancellation,
                work: VecDeque::new(),
                next_run_number: 2,
                thread_id: None,
                handle: None,
                wait_interest: 0,
                pending_delivery: Vec::new(),
                suppress_delivery: false,
                steers,
                steer_origins: VecDeque::new(),
                model_steer_accepted: false,
            });
            (id, conversation, work)
        };
        self.start_worker(id.clone(), conversation, work)?;
        Ok(id)
    }

    pub(crate) fn steer(&self, id: &str, message: &str) -> Result<String, String> {
        self.steer_with_origin(id, message, RunOrigin::PrivateUser)
    }

    pub(crate) fn steer_with_origin(
        &self,
        id: &str,
        message: &str,
        origin: RunOrigin,
    ) -> Result<String, String> {
        let message = validate_message(message, "message")?;
        let (enqueue, send_instead) = {
            let mut state = self.lock();
            let index = find_index(&state, id)?;
            let is_active = state.entries[index].snapshot.status.is_active();
            if is_active {
                let entry = &mut state.entries[index];
                if entry.snapshot.status == SubagentStatus::Canceling {
                    return Err(format!("{id} is canceling"));
                }
                if entry.work.len() + entry.snapshot.pending_steers.len() >= MAX_QUEUE_MESSAGES {
                    return Err(format!(
                        "{id} already has {MAX_QUEUE_MESSAGES} queued or steering messages"
                    ));
                }
                entry.steers.push(crate::provider::TurnInput {
                    text: message.clone(),
                    images: Vec::new(),
                });
                entry.snapshot.pending_steers.push(message.clone());
                entry.steer_origins.push_back(origin);
                self.shared.changed.notify_all();
                (true, false)
            } else {
                (false, true)
            }
        };
        if send_instead {
            return self.send(id, &message, origin);
        }
        let _ = enqueue;
        Ok(format!("steering {id}"))
    }

    pub(crate) fn send(
        &self,
        id: &str,
        message: &str,
        origin: RunOrigin,
    ) -> Result<String, String> {
        let message = validate_message(message, "message")?;
        let id = {
            let mut state = self.lock();
            let index = find_index(&state, id)?;
            let is_active = state.entries[index].snapshot.status.is_active();
            if is_active {
                let entry = &mut state.entries[index];
                if entry.snapshot.status == SubagentStatus::Canceling {
                    return Err(format!("{id} is canceling"));
                }
                if entry.work.len() + entry.snapshot.pending_steers.len() >= MAX_QUEUE_MESSAGES {
                    return Err(format!(
                        "{id} already has {MAX_QUEUE_MESSAGES} queued or steering messages"
                    ));
                }
                let run_number = entry.next_run_number;
                entry.next_run_number = entry.next_run_number.saturating_add(1);
                entry.work.push_back(WorkItem {
                    message: message.clone(),
                    origin,
                    run_number,
                });
                entry.snapshot.queued_messages.push(QueuedSubagentMessage {
                    text: message,
                    origin,
                });
                self.shared.changed.notify_all();
                return Ok(format!("queued message for {id}"));
            }
            if state.active >= state.limit {
                return Err(format!(
                    "subagent capacity is full ({}/{} running)",
                    state.active, state.limit
                ));
            }
            let entry = &mut state.entries[index];
            if entry.thread_id.is_none() {
                return Err(format!("{id} has no live worker"));
            }
            let run_number = entry.next_run_number;
            entry.next_run_number = entry.next_run_number.saturating_add(1);
            entry.work.push_back(WorkItem {
                message,
                origin,
                run_number,
            });
            entry.snapshot.status = SubagentStatus::Starting;
            entry.snapshot.current_activity = "starting".into();
            entry.snapshot.settled_at = None;
            entry.cancellation.clear();
            entry.suppress_delivery = false;
            let id = entry.snapshot.id.clone();
            state.active = state.active.saturating_add(1);
            self.shared.changed.notify_all();
            id
        };
        Ok(format!("restarted {id}"))
    }
}

pub(super) fn prune_settled(state: &mut State) {
    while state.entries.len() >= MAX_TRACKED_SUBAGENTS {
        let candidate = state
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                !entry.snapshot.status.is_active()
                    && entry.wait_interest == 0
                    && !state
                        .deferred
                        .iter()
                        .any(|delivery| delivery.id == entry.snapshot.id)
            })
            .min_by_key(|(_, entry)| {
                entry
                    .snapshot
                    .settled_at
                    .unwrap_or(entry.snapshot.created_at)
            })
            .map(|(index, _)| index);
        let Some(index) = candidate else {
            break;
        };
        state.entries.remove(index);
    }
}
