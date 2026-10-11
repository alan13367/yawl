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

/// A tracked child. Children beyond the concurrency limit wait in FIFO order
/// and start automatically when an earlier child settles.
pub(crate) struct Spawned {
    pub(crate) id: SubagentId,
    pub(crate) queued: bool,
}

impl SubagentManager {
    pub(crate) fn set_limit(&self, limit: usize) {
        let mut state = self.lock();
        state.limit = limit.clamp(1, 16);
        promote_queued(&mut state);
        self.shared.changed.notify_all();
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
            .map(|spawned| spawned.id)
    }

    pub(crate) fn spawn_with_model(
        &self,
        config: Config,
        parent_model: &str,
        name: Option<&str>,
        prompt: &str,
        preset: Option<&AgentPreset>,
        explicit_model: Option<&str>,
    ) -> Result<Spawned, String> {
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

        let (id, conversation, work, queued) = {
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
                model_steer_accepted: false,
                holds_slot: false,
                queued_sequence: 0,
            });
            let index = state.entries.len() - 1;
            let queued = !admit(&mut state, index);
            (id, conversation, work, queued)
        };
        self.start_worker(id.clone(), conversation, work)?;
        Ok(Spawned { id, queued })
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
        {
            let mut state = self.lock();
            let index = find_index(&state, id)?;
            let entry = &mut state.entries[index];
            if entry.snapshot.status.is_active() {
                if entry.snapshot.status == SubagentStatus::Canceling {
                    return Err(format!("{id} is canceling"));
                }
                if entry.work.len() + entry.snapshot.pending_steers.len() >= MAX_QUEUE_MESSAGES {
                    return Err(format!(
                        "{id} already has {MAX_QUEUE_MESSAGES} queued or steering messages"
                    ));
                }
                // The inbox and the pending list change together under the
                // manager lock, so acceptance order matches pending order.
                entry.steers.push(crate::provider::TurnInput {
                    text: message.clone(),
                    images: Vec::new(),
                });
                entry.snapshot.pending_steers.push(QueuedSubagentMessage {
                    text: message,
                    origin,
                });
                self.shared.changed.notify_all();
                return Ok(format!("steering {id}"));
            }
        }
        self.send(id, &message, origin)
    }

    pub(crate) fn send(
        &self,
        id: &str,
        message: &str,
        origin: RunOrigin,
    ) -> Result<String, String> {
        let message = validate_message(message, "message")?;
        let (id, queued) = {
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
            if state.entries[index].thread_id.is_none() {
                return Err(format!("{id} has no live worker"));
            }
            let entry = &mut state.entries[index];
            let run_number = entry.next_run_number;
            entry.next_run_number = entry.next_run_number.saturating_add(1);
            entry.work.push_back(WorkItem {
                message,
                origin,
                run_number,
            });
            entry.snapshot.settled_at = None;
            entry.cancellation.clear();
            entry.suppress_delivery = false;
            let id = entry.snapshot.id.clone();
            let queued = !admit(&mut state, index);
            self.shared.changed.notify_all();
            (id, queued)
        };
        Ok(if queued {
            format!("queued restart for {id}; it starts when a running subagent settles")
        } else {
            format!("restarted {id}")
        })
    }
}

/// Gives the entry at `index` a free concurrency slot, or a FIFO queue
/// position when every slot is busy or an earlier child is already waiting.
/// Returns whether it holds a slot.
fn admit(state: &mut State, index: usize) -> bool {
    let waiting = state
        .entries
        .iter()
        .any(|entry| entry.snapshot.status == SubagentStatus::Queued);
    let holds_slot = !waiting && state.running < state.limit;
    if holds_slot {
        state.running += 1;
    } else {
        state.queue_sequence = state.queue_sequence.saturating_add(1);
    }
    let queued_sequence = state.queue_sequence;
    state.active = state.active.saturating_add(1);
    let entry = &mut state.entries[index];
    entry.holds_slot = holds_slot;
    entry.queued_sequence = queued_sequence;
    entry.snapshot.status = if holds_slot {
        SubagentStatus::Starting
    } else {
        SubagentStatus::Queued
    };
    entry.snapshot.current_activity = entry.snapshot.status.label().into();
    holds_slot
}

/// Returns a slot held by the entry at `index` and hands free slots to the
/// oldest queued children. Callers notify waiters afterwards.
pub(super) fn release_slot(state: &mut State, index: usize) {
    let entry = &mut state.entries[index];
    if entry.holds_slot {
        entry.holds_slot = false;
        state.running = state.running.saturating_sub(1);
    }
    promote_queued(state);
}

pub(super) fn promote_queued(state: &mut State) {
    while state.running < state.limit {
        let Some(index) = state
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.snapshot.status == SubagentStatus::Queued)
            .min_by_key(|(_, entry)| entry.queued_sequence)
            .map(|(index, _)| index)
        else {
            break;
        };
        state.running += 1;
        let entry = &mut state.entries[index];
        entry.holds_slot = true;
        entry.snapshot.status = SubagentStatus::Starting;
        entry.snapshot.current_activity = SubagentStatus::Starting.label().into();
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
