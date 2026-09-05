use std::collections::{HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::agent::{Conversation, RunLimits};
use crate::cancellation::CancellationToken;
use crate::config::Config;
use crate::provider::UsageSummary;

use super::presets::AgentPreset;
use super::types::{
    MAX_ERROR_BYTES, MAX_FINAL_RESULT_BYTES, MAX_PROMPT_CHARS, MAX_QUEUE_MESSAGES,
    MAX_TRACKED_SUBAGENTS, QueuedSubagentMessage, RunOrigin, RunOutcome, SubagentId,
    SubagentSnapshot, SubagentStatus, SubagentTranscriptItem, bounded,
};

const MAX_WAIT_SECS: u64 = 300;
const CANCEL_WAIT: Duration = Duration::from_secs(5);
const SALVAGE_SNIPPET_BYTES: usize = 500;

fn wait_timeout(timeout_secs: Option<u64>) -> Result<Option<Duration>, String> {
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

#[derive(Clone)]
pub(crate) struct SubagentManager {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    session_id: String,
    next_id: u64,
    active: usize,
    limit: usize,
    entries: Vec<Entry>,
    deferred: VecDeque<DeferredResult>,
    settlement_sequence: u64,
    shutting_down: bool,
    /// Usage accumulated by finished subagent runs, kept after entries are
    /// pruned.
    total_child_usage: UsageSummary,
}

struct Entry {
    snapshot: SubagentSnapshot,
    cancellation: CancellationToken,
    work: VecDeque<WorkItem>,
    next_run_number: u64,
    thread_id: Option<usize>,
    handle: Option<JoinHandle<()>>,
    wait_interest: usize,
    pending_delivery: Vec<PendingDelivery>,
    suppress_delivery: bool,
    steers: crate::agent::SteerInbox,
}

#[derive(Clone)]
struct WorkItem {
    message: String,
    origin: RunOrigin,
    run_number: u64,
}

struct PendingDelivery {
    run_number: u64,
    outcome: RunOutcome,
    result: String,
    error: String,
}

#[derive(Debug, Clone)]
pub(crate) struct DeferredResult {
    pub(crate) id: SubagentId,
    pub(crate) name: String,
    pub(crate) run_number: u64,
    pub(crate) outcome: RunOutcome,
    pub(crate) result: String,
    pub(crate) error: String,
    sequence: u64,
}

impl SubagentManager {
    pub(crate) fn new(session_id: String, limit: usize) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    session_id,
                    next_id: 1,
                    active: 0,
                    limit: limit.clamp(1, 16),
                    entries: Vec::new(),
                    deferred: VecDeque::new(),
                    settlement_sequence: 0,
                    shutting_down: false,
                    total_child_usage: UsageSummary::default(),
                }),
                changed: Condvar::new(),
            }),
        }
    }

    pub(crate) fn set_limit(&self, limit: usize) {
        self.lock().limit = limit.clamp(1, 16);
    }

    pub(crate) fn spawn(
        &self,
        config: Config,
        parent_model: &str,
        name: Option<&str>,
        prompt: &str,
        preset: Option<&AgentPreset>,
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
        let model = resolve_model(&config, parent_model, preset_model)?;

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
                    super::names::generate_name(&existing, state.next_id)
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
            });
            (id, conversation, work)
        };
        self.start_worker(id.clone(), conversation, work)?;
        Ok(id)
    }

    pub(crate) fn steer(&self, id: &str, message: &str) -> Result<String, String> {
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
                self.shared.changed.notify_all();
                (true, false)
            } else {
                (false, true)
            }
        };
        if send_instead {
            return self.send(id, &message, RunOrigin::PrivateUser);
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

    pub(crate) fn wait(&self, ids: &[String], timeout_secs: Option<u64>) -> Result<String, String> {
        validate_id_list(ids)?;
        let timeout = wait_timeout(timeout_secs)?;
        let deadline = timeout.and_then(|timeout| Instant::now().checked_add(timeout));
        let mut state = self.lock();
        let indexes = resolve_indexes(&state, ids)?;
        let selected = indexes
            .iter()
            .map(|index| state.entries[*index].snapshot.id.clone())
            .collect::<Vec<_>>();
        for index in &indexes {
            state.entries[*index].wait_interest =
                state.entries[*index].wait_interest.saturating_add(1);
        }
        loop {
            if selected.iter().all(|id| {
                state
                    .entries
                    .iter()
                    .find(|entry| entry.snapshot.id == *id)
                    .is_none_or(|entry| !entry.snapshot.status.is_active())
            }) || crate::cancellation::interrupted()
                || deadline.is_some_and(|deadline| Instant::now() >= deadline)
            {
                break;
            }
            let remaining = deadline
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::from_millis(100))
                .min(Duration::from_millis(100));
            state = match self.shared.changed.wait_timeout(state, remaining) {
                Ok((state, _)) => state,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        let snapshots = selected
            .iter()
            .filter_map(|id| {
                state
                    .entries
                    .iter()
                    .find(|entry| entry.snapshot.id == *id)
                    .map(|entry| entry.snapshot.clone())
            })
            .collect::<Vec<_>>();
        for id in selected {
            if let Some(entry) = state
                .entries
                .iter_mut()
                .find(|entry| entry.snapshot.id == id)
            {
                entry.wait_interest = entry.wait_interest.saturating_sub(1);
            }
        }
        let mut consumed = state
            .deferred
            .iter()
            .filter(|delivery| ids.iter().any(|id| id == delivery.id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        state
            .deferred
            .retain(|delivery| !ids.iter().any(|id| id == delivery.id.as_str()));
        let mut output = format_snapshots(&snapshots);
        let active = snapshots
            .iter()
            .filter(|snapshot| snapshot.status.is_active())
            .count();
        if active > 0 {
            if crate::cancellation::interrupted() {
                output = format!(
                    "Wait interrupted with {active} of {} subagent(s) still \
                     running. This turn was canceled; the wait ended, not the work. \
                     Their results arrive automatically when they settle. Do not wait again \
                     or cancel a subagent because a wait was interrupted.\n\n{output}",
                    snapshots.len()
                );
            } else if let Some(timeout_secs) = timeout_secs {
                output = format!(
                    "Wait ended after {timeout_secs}s with {active} of {} subagent(s) still \
                     running. This is normal: subagent tasks often take 10+ minutes. Their results \
                     arrive automatically when they settle, so wait again or continue other work. \
                     Do not cancel a subagent because a wait timed out.\n\n{output}",
                    snapshots.len()
                );
            }
        }
        consumed.sort_by_key(|delivery| delivery.run_number);
        for delivery in consumed {
            // The snapshot section already carries the latest run's full text,
            // so only earlier settled runs need their own section.
            if snapshots.iter().any(|snapshot| {
                snapshot.id == delivery.id && snapshot.run_number == delivery.run_number
            }) {
                continue;
            }
            if !output.is_empty() {
                output.push_str("\n\n");
            }
            output.push_str(&format_deferred_result(&delivery));
        }
        Ok(output)
    }

    pub(crate) fn cancel(&self, ids: &[String], private: bool) -> Result<String, String> {
        validate_id_list(ids)?;
        let deadline = Instant::now().checked_add(CANCEL_WAIT);
        let mut state = self.lock();
        let indexes = resolve_indexes(&state, ids)?;
        let selected = indexes
            .iter()
            .map(|index| state.entries[*index].snapshot.id.clone())
            .collect::<Vec<_>>();
        let mut wake_threads = Vec::new();
        for index in &indexes {
            let entry = &mut state.entries[*index];
            entry.wait_interest = entry.wait_interest.saturating_add(1);
            if entry.snapshot.status.is_active() {
                entry.snapshot.status = SubagentStatus::Canceling;
                entry.snapshot.current_activity = "canceling".into();
                entry.work.clear();
                entry.snapshot.queued_messages.clear();
                entry.cancellation.cancel();
                if let Some(thread) = entry.thread_id {
                    wake_threads.push(thread);
                }
                if private {
                    entry.suppress_delivery = true;
                }
            }
        }
        if !private {
            state
                .deferred
                .retain(|delivery| !ids.iter().any(|id| id == delivery.id.as_str()));
        }
        for thread in wake_threads {
            crate::cancellation::wake_thread(thread);
        }
        loop {
            if selected.iter().all(|id| {
                state
                    .entries
                    .iter()
                    .find(|entry| entry.snapshot.id == *id)
                    .is_none_or(|entry| !entry.snapshot.status.is_active())
            }) || crate::cancellation::interrupted()
                || deadline.is_some_and(|deadline| Instant::now() >= deadline)
            {
                break;
            }
            let remaining = deadline
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::from_millis(100))
                .min(Duration::from_millis(100));
            state = match self.shared.changed.wait_timeout(state, remaining) {
                Ok((state, _)) => state,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        let snapshots = selected
            .iter()
            .filter_map(|id| {
                state
                    .entries
                    .iter()
                    .find(|entry| entry.snapshot.id == *id)
                    .map(|entry| entry.snapshot.clone())
            })
            .collect::<Vec<_>>();
        for id in selected {
            if let Some(entry) = state
                .entries
                .iter_mut()
                .find(|entry| entry.snapshot.id == id)
            {
                entry.wait_interest = entry.wait_interest.saturating_sub(1);
            }
        }
        Ok(format_snapshots(&snapshots))
    }

    pub(crate) fn list(&self, id: Option<&str>) -> Result<String, String> {
        let state = self.lock();
        if let Some(id) = id {
            let index = find_index(&state, id)?;
            return Ok(format_detailed_snapshot(&state.entries[index].snapshot));
        }
        if state.entries.is_empty() {
            return Ok("no tracked subagents".into());
        }
        let now = Instant::now();
        let mut output = String::new();
        for entry in &state.entries {
            let snapshot = &entry.snapshot;
            let percentage = snapshot
                .context_tokens
                .saturating_mul(100)
                .checked_div(snapshot.context_window)
                .unwrap_or(0);
            output.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}% ({}/{})\t{}\tqueued {}\n",
                snapshot.id,
                snapshot.status.label(),
                snapshot.name,
                snapshot.model,
                percentage,
                snapshot.context_tokens,
                snapshot.context_window,
                format_duration(snapshot.elapsed(now)),
                snapshot.queued_messages.len()
            ));
        }
        Ok(output.trim_end().to_string())
    }

    #[cfg(test)]
    pub(crate) fn snapshots(&self) -> Vec<SubagentSnapshot> {
        self.lock()
            .entries
            .iter()
            .map(|entry| entry.snapshot.clone())
            .collect()
    }

    pub(crate) fn display_snapshots(&self, selected: Option<&str>) -> (Vec<SubagentSnapshot>, u64) {
        let state = self.lock();
        let snapshots = state
            .entries
            .iter()
            .map(|entry| {
                entry
                    .snapshot
                    .display_snapshot(selected == Some(entry.snapshot.id.as_str()))
            })
            .collect();
        (snapshots, state.total_child_usage.tokens.total_tokens())
    }

    /// Session-wide usage tokens across every subagent run.
    pub(crate) fn total_child_tokens(&self) -> u64 {
        self.lock().total_child_usage.tokens.total_tokens()
    }

    pub(crate) fn total_child_usage(&self) -> UsageSummary {
        self.lock().total_child_usage
    }

    pub(crate) fn active_count(&self) -> usize {
        self.lock().active
    }

    /// Blocks until every active subagent settles, the process is
    /// interrupted, or `timeout_secs` elapses. Returns `true` only when all
    /// subagents settled. Used by print mode, which has no event loop to
    /// receive background deliveries on.
    pub(crate) fn wait_all(&self, timeout_secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
        let mut state = self.lock();
        loop {
            if state.active == 0 || crate::cancellation::interrupted() {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            state = match self
                .shared
                .changed
                .wait_timeout(state, remaining.min(Duration::from_millis(100)))
            {
                Ok((state, _)) => state,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        state.active == 0
    }

    pub(crate) fn has_deferred(&self) -> bool {
        !self.lock().deferred.is_empty()
    }

    pub(crate) fn drain_deferred(&self) -> Vec<DeferredResult> {
        let mut state = self.lock();
        let mut deliveries = state.deferred.drain(..).collect::<Vec<_>>();
        deliveries.sort_by_key(|delivery| delivery.sequence);
        deliveries
    }

    pub(crate) fn restore_deferred(&self, deliveries: Vec<DeferredResult>) {
        let mut state = self.lock();
        let mut combined = state.deferred.drain(..).collect::<Vec<_>>();
        combined.extend(deliveries);
        combined.sort_by_key(|delivery| delivery.sequence);
        state.deferred = combined.into();
    }

    /// Pushes a fabricated settled-run delivery. Test-only: real deliveries
    /// come from worker threads. `id_sequence` picks the `sa-N` id.
    #[cfg(test)]
    pub(crate) fn push_test_deferred(
        &self,
        id_sequence: u64,
        name: &str,
        outcome: RunOutcome,
        result: &str,
    ) {
        let mut state = self.lock();
        state.settlement_sequence = state.settlement_sequence.saturating_add(1);
        let sequence = state.settlement_sequence;
        state.deferred.push_back(DeferredResult {
            id: SubagentId::new(id_sequence),
            name: name.into(),
            run_number: 1,
            outcome,
            result: result.into(),
            error: String::new(),
            sequence,
        });
    }

    pub(crate) fn shutdown_and_discard(&self) {
        let (threads, handles) = {
            let mut state = self.lock();
            state.shutting_down = true;
            let mut threads = Vec::new();
            let mut handles = Vec::new();
            for entry in &mut state.entries {
                if entry.snapshot.status.is_active() {
                    entry.snapshot.status = SubagentStatus::Canceling;
                    entry.cancellation.cancel();
                    entry.work.clear();
                    entry.snapshot.queued_messages.clear();
                    if let Some(thread) = entry.thread_id {
                        threads.push(thread);
                    }
                }
                if let Some(handle) = entry.handle.take() {
                    handles.push(handle);
                }
            }
            (threads, handles)
        };
        for thread in threads {
            crate::cancellation::wake_thread(thread);
        }
        self.shared.changed.notify_all();
        for handle in handles {
            let _ = handle.join();
        }
        let mut state = self.lock();
        state.entries.clear();
        state.deferred.clear();
        state.active = 0;
        self.shared.changed.notify_all();
    }

    pub(crate) fn interrupt_all(&self) {
        let threads = {
            let mut state = self.lock();
            let mut threads = Vec::new();
            for entry in &mut state.entries {
                if entry.snapshot.status.is_active() {
                    entry.snapshot.status = SubagentStatus::Canceling;
                    entry.snapshot.current_activity = "canceling".into();
                    entry.work.clear();
                    entry.snapshot.queued_messages.clear();
                    entry.suppress_delivery = true;
                    entry.cancellation.cancel();
                    if let Some(thread) = entry.thread_id {
                        threads.push(thread);
                    }
                }
            }
            threads
        };
        for thread in threads {
            crate::cancellation::wake_thread(thread);
        }
        self.shared.changed.notify_all();
    }

    fn start_worker(
        &self,
        id: SubagentId,
        mut conversation: Conversation,
        first_work: WorkItem,
    ) -> Result<(), String> {
        let manager = self.clone();
        let worker_id = id.clone();
        let spawn = std::thread::Builder::new()
            .name(format!("yawl-{id}"))
            .spawn(move || {
                manager.worker_started(&worker_id);
                let result = catch_unwind(AssertUnwindSafe(|| {
                    manager.worker_loop(&worker_id, &mut conversation, first_work)
                }));
                match result {
                    Ok(()) => manager.worker_stopped(&worker_id),
                    Err(_) => manager.worker_panicked(&worker_id),
                }
            });
        match spawn {
            Ok(handle) => {
                let mut state = self.lock();
                if let Some(index) = state
                    .entries
                    .iter()
                    .position(|entry| entry.snapshot.id == id)
                {
                    state.entries[index].handle = Some(handle);
                }
                Ok(())
            }
            Err(error) => {
                let mut state = self.lock();
                if let Some(index) = state
                    .entries
                    .iter()
                    .position(|entry| entry.snapshot.id == id)
                {
                    let entry = &mut state.entries[index];
                    entry.snapshot.status = SubagentStatus::Failed;
                    entry.snapshot.error = bounded(
                        &format!("failed to create subagent worker: {error}"),
                        super::types::MAX_ERROR_BYTES,
                    );
                    entry.snapshot.settled_at = Some(Instant::now());
                    state.active = state.active.saturating_sub(1);
                }
                self.shared.changed.notify_all();
                Err(format!("failed to create subagent worker: {error}"))
            }
        }
    }

    fn worker_started(&self, id: &SubagentId) {
        let mut state = self.lock();
        if let Some(entry) = state
            .entries
            .iter_mut()
            .find(|entry| entry.snapshot.id == *id)
        {
            entry.thread_id = Some(crate::cancellation::native_thread_id());
            entry.snapshot.started_at = Some(Instant::now());
        }
        self.shared.changed.notify_all();
    }

    fn worker_loop(&self, id: &SubagentId, conversation: &mut Conversation, first_work: WorkItem) {
        let mut next_work = Some(first_work);
        loop {
            let Some(work) = next_work.take().or_else(|| self.wait_for_work(id)) else {
                return;
            };
            if !self.begin_work(id, &work) {
                return;
            }
            let run_model = conversation.model().to_string();
            let result = conversation.run_turn_preserving_cancellation(
                Some(work.message.clone()),
                &mut |event| {
                    let mut state = self.lock();
                    apply_turn_event(&mut state, id, event);
                },
            );
            let (outcome, error) = match result {
                Ok(true) => (RunOutcome::Completed, None),
                Ok(false) | Err(crate::error::Error::Interrupted) => {
                    (RunOutcome::Interrupted, None)
                }
                // Attribution: a failed child rarely knows which provider or
                // model actually errored, so the label rides the error text.
                Err(error) => (RunOutcome::Failed, Some(format!("[{run_model}] {error}"))),
            };
            let mut leftovers = conversation.take_unaccepted_steers();
            let final_result = conversation.latest_turn_result();
            let mut state = self.lock();
            // Steering also takes the manager lock before pushing into the
            // inbox. Draining again while holding it closes the boundary
            // race between a turn ending and the settled state becoming
            // visible.
            leftovers.extend(conversation.take_unaccepted_steers());
            let Some(index) = state
                .entries
                .iter()
                .position(|entry| entry.snapshot.id == *id)
            else {
                return;
            };
            let entry = &mut state.entries[index];
            entry
                .snapshot
                .finish_turn(outcome, &final_result, error.as_deref());
            // finish_turn has already folded any live partial text into the
            // transcript, so the salvage scan sees the complete history.
            let delivered_result = match outcome {
                RunOutcome::Interrupted => {
                    let salvaged = salvage_result(&entry.snapshot, &final_result);
                    entry.snapshot.latest_final_result = salvaged.clone();
                    salvaged
                }
                _ => bounded(&final_result, MAX_FINAL_RESULT_BYTES),
            };
            if work.origin == RunOrigin::Model && !entry.suppress_delivery {
                entry.pending_delivery.push(PendingDelivery {
                    run_number: work.run_number,
                    outcome,
                    result: delivered_result,
                    error: error
                        .as_deref()
                        .map_or_else(String::new, |text| bounded(text, MAX_ERROR_BYTES)),
                });
            }
            entry.snapshot.pending_steers.clear();
            for leftover in leftovers {
                if entry.work.len() >= MAX_QUEUE_MESSAGES {
                    break;
                }
                let run_number = entry.next_run_number;
                entry.next_run_number = entry.next_run_number.saturating_add(1);
                entry.work.push_back(WorkItem {
                    message: leftover.text.clone(),
                    origin: RunOrigin::PrivateUser,
                    run_number,
                });
                entry.snapshot.queued_messages.push(QueuedSubagentMessage {
                    text: leftover.text,
                    origin: RunOrigin::PrivateUser,
                });
            }
            let canceling = entry.snapshot.status == SubagentStatus::Canceling;
            let next = entry.work.pop_front();
            if !entry.snapshot.queued_messages.is_empty() {
                entry.snapshot.queued_messages.remove(0);
            }
            entry.cancellation.clear();
            if !canceling && let Some(next) = next {
                next_work = Some(next);
                self.shared.changed.notify_all();
                continue;
            }
            settle_entry(&mut state, index, outcome, self.shared.as_ref());
        }
    }

    fn wait_for_work(&self, id: &SubagentId) -> Option<WorkItem> {
        let mut state = self.lock();
        loop {
            if state.shutting_down {
                return None;
            }
            let index = state
                .entries
                .iter()
                .position(|entry| entry.snapshot.id == *id)?;
            if state.entries[index].snapshot.status == SubagentStatus::Canceling {
                state.entries[index].snapshot.latest_outcome = Some(RunOutcome::Interrupted);
                settle_entry(
                    &mut state,
                    index,
                    RunOutcome::Interrupted,
                    self.shared.as_ref(),
                );
                return None;
            }
            let entry = &mut state.entries[index];
            if let Some(work) = entry.work.pop_front() {
                if !entry.snapshot.queued_messages.is_empty() {
                    entry.snapshot.queued_messages.remove(0);
                }
                return Some(work);
            }
            state = match self.shared.changed.wait(state) {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
    }

    fn begin_work(&self, id: &SubagentId, work: &WorkItem) -> bool {
        let mut state = self.lock();
        let Some(index) = state
            .entries
            .iter()
            .position(|entry| entry.snapshot.id == *id)
        else {
            return false;
        };
        if state.entries[index].snapshot.status == SubagentStatus::Canceling {
            state.entries[index].snapshot.latest_outcome = Some(RunOutcome::Interrupted);
            settle_entry(
                &mut state,
                index,
                RunOutcome::Interrupted,
                self.shared.as_ref(),
            );
            return false;
        }
        state.entries[index]
            .snapshot
            .begin_turn(&work.message, work.origin, work.run_number);
        true
    }

    fn worker_stopped(&self, id: &SubagentId) {
        let mut state = self.lock();
        if let Some(entry) = state
            .entries
            .iter_mut()
            .find(|entry| entry.snapshot.id == *id)
        {
            entry.thread_id = None;
        }
        self.shared.changed.notify_all();
    }

    fn worker_panicked(&self, id: &SubagentId) {
        let mut state = self.lock();
        let Some(index) = state
            .entries
            .iter()
            .position(|entry| entry.snapshot.id == *id)
        else {
            return;
        };
        let was_active = state.entries[index].snapshot.status.is_active();
        let entry = &mut state.entries[index];
        entry.thread_id = None;
        entry.snapshot.status = SubagentStatus::Failed;
        entry.snapshot.error = "subagent worker panicked".into();
        entry.snapshot.latest_outcome = Some(RunOutcome::Failed);
        entry.snapshot.settled_at = Some(Instant::now());
        if was_active
            && entry.snapshot.origin == RunOrigin::Model
            && !entry.suppress_delivery
            && !entry
                .pending_delivery
                .iter()
                .any(|delivery| delivery.run_number == entry.snapshot.run_number)
        {
            entry.pending_delivery.push(PendingDelivery {
                run_number: entry.snapshot.run_number,
                outcome: RunOutcome::Failed,
                result: String::new(),
                error: "subagent worker panicked".into(),
            });
        }
        if was_active {
            state.active = state.active.saturating_sub(1);
        }
        flush_pending_deliveries(&mut state, index);
        self.shared.changed.notify_all();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.shared.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

fn apply_turn_event(state: &mut State, id: &SubagentId, event: crate::agent::TurnEvent<'_>) {
    let Some(index) = state
        .entries
        .iter()
        .position(|entry| entry.snapshot.id == *id)
    else {
        return;
    };
    match &event {
        crate::agent::TurnEvent::Usage { request_usage, .. } => {
            state.total_child_usage.record(*request_usage);
        }
        crate::agent::TurnEvent::Compacted { .. } => {
            state.total_child_usage.record_cache_reset();
        }
        _ => {}
    }
    state.entries[index].snapshot.apply_event(event);
}

fn settle_entry(state: &mut State, index: usize, outcome: RunOutcome, shared: &Shared) {
    let shutting_down = state.shutting_down;
    let entry = &mut state.entries[index];
    entry.snapshot.status = if outcome == RunOutcome::Failed {
        SubagentStatus::Failed
    } else {
        SubagentStatus::Done
    };
    entry.snapshot.latest_outcome.get_or_insert(outcome);
    entry.snapshot.settled_at = Some(Instant::now());
    entry.snapshot.current_tool = None;
    entry.snapshot.live_assistant.clear();
    entry.snapshot.live_reasoning.clear();
    entry.snapshot.current_activity.clear();
    entry.snapshot.pending_steers.clear();
    if shutting_down {
        entry.pending_delivery.clear();
    }
    state.active = state.active.saturating_sub(1);
    if !shutting_down {
        flush_pending_deliveries(state, index);
    }
    shared.changed.notify_all();
}

fn flush_pending_deliveries(state: &mut State, index: usize) {
    let pending = std::mem::take(&mut state.entries[index].pending_delivery);
    let id = state.entries[index].snapshot.id.clone();
    let name = state.entries[index].snapshot.name.clone();
    for delivery in pending {
        state.settlement_sequence = state.settlement_sequence.saturating_add(1);
        state.deferred.push_back(DeferredResult {
            id: id.clone(),
            name: name.clone(),
            run_number: delivery.run_number,
            outcome: delivery.outcome,
            result: delivery.result,
            error: delivery.error,
            sequence: state.settlement_sequence,
        });
    }
}

fn validate_name(name: &str) -> Result<String, String> {
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

fn validate_message(message: &str, label: &str) -> Result<String, String> {
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

fn resolve_model(
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

/// Fails at spawn time instead of as a dead run later. Resolution instantiates
/// the provider (which checks credentials and may perform an OAuth refresh for
/// near-expiry Codex tokens).
fn validate_resolved_model(config: &Config, model: String) -> Result<String, String> {
    crate::provider::resolve(&model, config)
        .map(|_| model.clone())
        .map_err(|error| format!("model '{model}' is not usable: {error}"))
}

fn validate_id_list(ids: &[String]) -> Result<(), String> {
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

fn find_index(state: &State, id: &str) -> Result<usize, String> {
    state
        .entries
        .iter()
        .position(|entry| entry.snapshot.id.as_str() == id)
        .ok_or_else(|| unknown_ids(state, &[id.to_string()]))
}

fn resolve_indexes(state: &State, ids: &[String]) -> Result<Vec<usize>, String> {
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

fn unknown_ids(state: &State, unknown: &[String]) -> String {
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

fn prune_settled(state: &mut State) {
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

fn format_snapshots(snapshots: &[SubagentSnapshot]) -> String {
    let mut output = String::new();
    for snapshot in snapshots {
        if !output.is_empty() {
            output.push_str("\n\n");
        }
        output.push_str(&format_detailed_snapshot(snapshot));
    }
    output
}

fn format_deferred_result(delivery: &DeferredResult) -> String {
    let status = match delivery.outcome {
        RunOutcome::Completed => "completed",
        RunOutcome::Failed => "failed",
        RunOutcome::Interrupted => "interrupted",
    };
    let body = if delivery.error.is_empty() {
        delivery.result.clone()
    } else if delivery.result.is_empty() {
        delivery.error.clone()
    } else {
        format!("{}\n\nError: {}", delivery.result, delivery.error)
    };
    format!(
        "{} [{}] {} (run {})\n{}",
        delivery.id, status, delivery.name, delivery.run_number, body
    )
}

fn format_detailed_snapshot(snapshot: &SubagentSnapshot) -> String {
    let percentage = snapshot
        .context_tokens
        .saturating_mul(100)
        .checked_div(snapshot.context_window)
        .unwrap_or(0);
    let mut output = format!(
        "{} [{}] {}\nagent: {}\nmodel: {}\ninitial task: {}\ncontext: {}% ({}/{})\nelapsed: {}\nactivity: {}\noutcome: {}\nturns: {}\nrequests: {}\nqueued: {}",
        snapshot.id,
        snapshot.status.label(),
        snapshot.name,
        snapshot.agent,
        snapshot.model,
        bounded(&snapshot.initial_prompt.replace('\n', " "), 240),
        percentage,
        snapshot.context_tokens,
        snapshot.context_window,
        format_duration(snapshot.elapsed(Instant::now())),
        if snapshot.current_activity.is_empty() {
            "idle"
        } else {
            &snapshot.current_activity
        },
        match snapshot.latest_outcome {
            Some(RunOutcome::Completed) => "completed",
            Some(RunOutcome::Failed) => "failed",
            Some(RunOutcome::Interrupted) => "interrupted",
            None => "pending",
        },
        snapshot.completed_turns,
        snapshot.requests,
        snapshot.queued_messages.len()
    );
    if !snapshot.error.is_empty() {
        output.push_str("\nerror: ");
        output.push_str(&snapshot.error);
    }
    let result = final_output(snapshot);
    if !result.is_empty() {
        output.push_str("\nresult:\n");
        output.push_str(&result);
    }
    output
}

/// The text reported to the orchestrator once a run settles: the complete
/// final response, the live partial answer while streaming, or the error.
fn final_output(snapshot: &SubagentSnapshot) -> String {
    let source = if !snapshot.live_assistant.is_empty() {
        &snapshot.live_assistant
    } else if !snapshot.latest_final_result.is_empty() {
        &snapshot.latest_final_result
    } else {
        &snapshot.error
    };
    source.clone()
}

/// Interrupted runs deliver a salvage envelope instead of a possibly empty
/// final result: the request count plus the last assistant text, so partial
/// work still reaches the parent.
fn salvage_result(snapshot: &SubagentSnapshot, final_result: &str) -> String {
    let requests = snapshot.requests;
    let result = final_result.trim();
    if !result.is_empty() {
        return bounded(
            &format!("[cancelled after {requests} requests]\n{result}"),
            MAX_FINAL_RESULT_BYTES,
        );
    }
    let last_activity = snapshot
        .transcript
        .iter()
        .rev()
        .find_map(|item| match item.as_ref() {
            SubagentTranscriptItem::Assistant(text) => Some(text.as_str()),
            _ => None,
        });
    match last_activity {
        Some(text) if !text.trim().is_empty() => bounded(
            &format!(
                "[cancelled after {requests} requests — last activity: \"{}\"]",
                bounded(text.trim(), SALVAGE_SNIPPET_BYTES)
            ),
            MAX_FINAL_RESULT_BYTES,
        ),
        _ => format!("[cancelled after {requests} requests]"),
    }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3600 {
        format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    use super::*;
    use crate::config::ProviderConfig;

    fn config() -> Config {
        Config {
            model: Some("parent".into()),
            subagents: true,
            auto_compact: false,
            ..Config::test_default()
        }
    }

    fn provider_config(base_url: String) -> Config {
        let mut config = config();
        config.model = Some("local:model".into());
        config.providers.insert(
            "local".into(),
            ProviderConfig {
                base_url,
                api: "openai-completions".into(),
                api_key: None,
                auth_header: Some(false),
                headers: HashMap::new(),
                models: Vec::new(),
                compat: crate::config::OpenAiCompatibility::default(),
            },
        );
        config
    }

    fn test_entry(status: SubagentStatus) -> Entry {
        let conversation = Conversation::memory(config(), "parent".into(), "session-sa-1".into());
        let cancellation = conversation.cancellation_token();
        let mut snapshot = SubagentSnapshot::new(
            SubagentId::new(1),
            "agent".into(),
            "default".into(),
            "task".into(),
            "parent".into(),
            100,
        );
        snapshot.status = status;
        Entry {
            snapshot,
            cancellation,
            work: VecDeque::new(),
            next_run_number: 2,
            thread_id: None,
            handle: None,
            wait_interest: 0,
            pending_delivery: Vec::new(),
            suppress_delivery: false,
            steers: crate::agent::SteerInbox::default(),
        }
    }

    #[test]
    fn usage_is_counted_before_an_active_subagent_settles() {
        let manager = SubagentManager::new("session".into(), 1);
        let id = SubagentId::new(1);
        {
            let mut state = manager.lock();
            state.entries.push(test_entry(SubagentStatus::Running));
            apply_turn_event(
                &mut state,
                &id,
                crate::agent::TurnEvent::Usage {
                    context_tokens: 120,
                    context_window: 1_000,
                    request_usage: crate::provider::TokenUsage {
                        input_tokens: 100,
                        output_tokens: 20,
                        cached_input_tokens: 75,
                        cache_write_input_tokens: 0,
                        cache_details_reported: true,
                    },
                    session_usage: crate::provider::UsageSummary::default(),
                },
            );
            assert_eq!(state.entries[0].snapshot.status, SubagentStatus::Running);
        }

        let usage = manager.total_child_usage();
        assert_eq!(usage.requests, 1);
        assert_eq!(usage.tokens.total_tokens(), 120);
        assert_eq!(usage.cache_hit_percent(), 75);
        manager.shutdown_and_discard();
    }

    fn read_request(stream: &mut TcpStream) -> std::io::Result<String> {
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        let mut wanted = None;
        loop {
            let read = stream.read(&mut buffer)?;
            if read == 0 {
                return Ok(String::new());
            }
            request.extend_from_slice(&buffer[..read]);
            if wanted.is_none()
                && let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
            {
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                wanted = Some(header_end + 4 + content_length);
            }
            if wanted.is_some_and(|wanted| request.len() >= wanted) {
                return Ok(String::from_utf8_lossy(&request).into_owned());
            }
        }
    }

    fn write_response(stream: &mut TcpStream, text: &str) -> std::io::Result<()> {
        let body = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}},\"finish_reason\":null}}]}}\n\ndata: [DONE]\n\n"
        );
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )?;
        stream.flush()
    }

    fn write_multiline_response(stream: &mut TcpStream, text: &str) -> std::io::Result<()> {
        let event = serde_json::json!({
            "choices": [{"delta": {"content": text}, "finish_reason": null}]
        });
        let body = format!("data: {event}\n\ndata: [DONE]\n\n");
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )?;
        stream.flush()
    }

    /// Streams `text` followed by an unknown-tool call and a usage event, so
    /// the child keeps issuing requests while burning budget.
    fn write_tool_call_response(
        stream: &mut TcpStream,
        text: &str,
        call_id: &str,
    ) -> std::io::Result<()> {
        let mut body = String::new();
        if !text.is_empty() {
            let event = serde_json::json!({"choices": [{"delta": {"content": text}}]});
            body.push_str(&format!("data: {event}\n\n"));
        }
        let call = serde_json::json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": call_id,
                "type": "function",
                "function": {"name": "noop_probe", "arguments": "{}"}
            }]}}]
        });
        body.push_str(&format!("data: {call}\n\n"));
        let usage = serde_json::json!({
            "choices": [],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2}
        });
        body.push_str(&format!("data: {usage}\n\n"));
        body.push_str("data: [DONE]\n\n");
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )?;
        stream.flush()
    }

    #[test]
    fn wait_reports_the_complete_final_result() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
        let base_url = format!(
            "http://{}/v1",
            listener.local_addr().expect("test provider address")
        );
        let answer = (1..=30)
            .map(|line| format!("summary line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("subagent provider connection");
            read_request(&mut stream).expect("subagent provider request");
            write_multiline_response(&mut stream, &answer).expect("subagent provider response");
        });
        let manager = SubagentManager::new("session".into(), 1);
        let id = manager
            .spawn(
                provider_config(base_url),
                "local:model",
                Some("scanner"),
                "scan the library",
                None,
            )
            .expect("subagent spawn");
        let waited = manager
            .wait(&[id.to_string()], Some(5))
            .expect("subagent should settle");

        assert!(waited.contains("summary line 01"));
        assert!(
            waited.contains("summary line 30"),
            "wait output should carry the complete answer; got:\n{waited}"
        );
        assert!(!manager.has_deferred());
        server.join().expect("provider server should exit");
        manager.shutdown_and_discard();
    }

    #[test]
    fn wait_reports_earlier_run_results_after_a_follow_up() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
        let base_url = format!(
            "http://{}/v1",
            listener.local_addr().expect("test provider address")
        );
        let server = std::thread::spawn(move || {
            for response in ["first full result", "second full result"] {
                let (mut stream, _) = listener.accept().expect("subagent provider connection");
                read_request(&mut stream).expect("subagent provider request");
                write_response(&mut stream, response).expect("subagent provider response");
            }
        });
        let manager = SubagentManager::new("session".into(), 1);
        let id = manager
            .spawn(
                provider_config(base_url),
                "local:model",
                Some("reused"),
                "first task",
                None,
            )
            .expect("initial subagent spawn");
        manager
            .send(id.as_str(), "follow-up", RunOrigin::Model)
            .expect("follow-up should queue or restart");
        let waited = manager
            .wait(&[id.to_string()], Some(5))
            .expect("both subagent runs should settle");

        assert!(
            waited.contains("first full result"),
            "earlier run results must survive into the wait; got:\n{waited}"
        );
        assert!(waited.contains("second full result"));
        assert!(!manager.has_deferred());
        server.join().expect("provider server should exit");
        manager.shutdown_and_discard();
    }

    #[test]
    fn model_precedence_prefers_config_then_parent() {
        let mut config = provider_config("http://127.0.0.1:9/v1".into());
        config.subagent_model = "local:configured".into();
        assert_eq!(
            resolve_model(&config, "local:parent", None).expect("configured model"),
            "local:configured"
        );
        config.subagent_model = "inherit".into();
        assert_eq!(
            resolve_model(&config, "local:parent", None).expect("inherited model"),
            "local:parent"
        );
    }

    #[test]
    fn unresolvable_configured_models_are_rejected_at_spawn_time() {
        let mut config = config();
        config.providers.insert(
            "broken".into(),
            ProviderConfig {
                base_url: String::new(),
                api: "openai-completions".into(),
                api_key: None,
                auth_header: Some(false),
                headers: HashMap::new(),
                models: Vec::new(),
                compat: crate::config::OpenAiCompatibility::default(),
            },
        );
        config.subagent_model = "broken:model".into();
        let error = resolve_model(&config, "local:parent", None)
            .expect_err("unusable provider models must fail fast");
        assert!(
            error.contains("'broken:model' is not usable"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn unknown_id_errors_include_known_ids() {
        let manager = SubagentManager::new("session".into(), 3);
        let error = manager
            .list(Some("sa-9"))
            .expect_err("unknown ID should fail");
        assert!(error.contains("known IDs: (none)"));
    }

    #[test]
    fn id_lists_reject_duplicates() {
        let ids = vec!["sa-1".to_string(), "sa-1".to_string()];
        assert!(validate_id_list(&ids).is_err());
    }

    #[test]
    fn omitted_wait_timeout_has_no_deadline() {
        assert_eq!(wait_timeout(None), Ok(None));
        assert_eq!(wait_timeout(Some(7)), Ok(Some(Duration::from_secs(7))));
    }

    #[test]
    fn wait_without_timeout_blocks_until_every_selected_subagent_settles() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
        let base_url = format!(
            "http://{}/v1",
            listener.local_addr().expect("test provider address")
        );
        let (ready_tx, ready_rx) = mpsc::channel();
        let (first_release_tx, first_release_rx) = mpsc::channel();
        let (second_release_tx, second_release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().expect("first provider connection");
            read_request(&mut first).expect("first provider request");
            ready_tx.send(()).expect("first provider ready");

            let (mut second, _) = listener.accept().expect("second provider connection");
            read_request(&mut second).expect("second provider request");
            ready_tx.send(()).expect("second provider ready");

            first_release_rx.recv().expect("first provider release");
            write_response(&mut first, "first result").expect("first provider response");
            second_release_rx.recv().expect("second provider release");
            write_response(&mut second, "second result").expect("second provider response");
        });
        let manager = SubagentManager::new("session".into(), 2);
        let config = provider_config(base_url);
        let first = manager
            .spawn(
                config.clone(),
                "local:model",
                Some("first"),
                "first task",
                None,
            )
            .expect("first subagent spawn");
        let second = manager
            .spawn(config, "local:model", Some("second"), "second task", None)
            .expect("second subagent spawn");
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("first worker should reach the provider");
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("second worker should reach the provider");

        let (wait_tx, wait_rx) = mpsc::channel();
        let wait_manager = manager.clone();
        let wait = std::thread::spawn(move || {
            let result = wait_manager.wait(&[first.to_string(), second.to_string()], None);
            wait_tx.send(result).expect("wait result receiver");
        });

        first_release_tx.send(()).expect("release first provider");
        assert!(
            wait_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "the wait must remain blocked while any selected subagent is active"
        );

        second_release_tx.send(()).expect("release second provider");
        let output = wait_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("wait should finish after both providers")
            .expect("wait result");
        assert!(output.contains("first result"));
        assert!(output.contains("second result"));
        assert!(!output.contains("Wait ended after"));

        wait.join().expect("wait thread should exit");
        server.join().expect("provider server should exit");
        manager.shutdown_and_discard();
    }

    #[test]
    fn omitted_spawn_names_are_generated_and_supplied_names_are_kept() {
        let config = provider_config("http://127.0.0.1:9/v1".into());
        let manager = SubagentManager::new("session".into(), 3);
        manager
            .spawn(config.clone(), "local:model", None, "task one", None)
            .expect("spawn without a name");
        manager
            .spawn(config, "local:model", None, "task two", None)
            .expect("second spawn without a name");
        manager
            .spawn(
                provider_config("http://127.0.0.1:9/v1".into()),
                "local:model",
                Some("  custom  "),
                "task three",
                None,
            )
            .expect("spawn with an explicit name");

        let snapshots = manager.snapshots();
        let names = snapshots
            .iter()
            .map(|snapshot| snapshot.name.as_str())
            .collect::<Vec<_>>();
        assert!(
            names.contains(&"custom"),
            "supplied names should be trimmed and kept; got {names:?}"
        );
        let generated = names
            .iter()
            .filter(|name| **name != "custom")
            .collect::<Vec<_>>();
        assert_eq!(generated.len(), 2);
        assert!(
            generated[0] != generated[1],
            "generated handles must be unique; got {names:?}"
        );
        assert!(generated.iter().all(|name| !name.is_empty()));
        manager.shutdown_and_discard();
    }

    #[test]
    fn wait_format_reports_every_id_with_complete_results() {
        let snapshots = (1..=MAX_TRACKED_SUBAGENTS as u64)
            .map(|sequence| {
                let mut snapshot = SubagentSnapshot::new(
                    SubagentId::new(sequence),
                    format!("agent {sequence}"),
                    "default".into(),
                    "p".repeat(240),
                    "model".into(),
                    100,
                );
                snapshot.latest_final_result = format!("result {sequence} tail");
                snapshot.status = SubagentStatus::Done;
                snapshot
            })
            .collect::<Vec<_>>();

        let output = format_snapshots(&snapshots);

        for sequence in 1..=MAX_TRACKED_SUBAGENTS as u64 {
            assert!(output.contains(&format!("sa-{sequence} [done]")));
            assert!(output.contains(&format!("result {sequence} tail")));
        }
    }

    #[test]
    fn active_capacity_and_queue_limits_are_reserved_synchronously() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
        let base_url = format!(
            "http://{}/v1",
            listener.local_addr().expect("test provider address")
        );
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("subagent provider connection");
            read_request(&mut stream).expect("subagent provider request");
            ready_tx.send(()).expect("provider ready signal");
            release_rx.recv().expect("provider release signal");
            write_response(&mut stream, "done").expect("subagent provider response");
        });
        let config = provider_config(base_url);
        let manager = SubagentManager::new("session".into(), 1);
        let id = manager
            .spawn(config.clone(), "local:model", Some("first"), "work", None)
            .expect("first spawn should reserve the only slot");
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("first worker should reach the provider");

        let error = manager
            .spawn(config, "local:model", Some("second"), "work", None)
            .expect_err("a simultaneous spawn must not exceed capacity");
        assert!(error.contains("capacity is full"));
        for index in 0..MAX_QUEUE_MESSAGES {
            manager
                .send(id.as_str(), &format!("queued {index}"), RunOrigin::Model)
                .expect("messages through the queue limit should be accepted");
        }
        assert!(
            manager
                .send(id.as_str(), "one too many", RunOrigin::Model)
                .is_err()
        );

        manager.interrupt_all();
        release_tx.send(()).expect("release provider response");
        manager
            .wait(&[id.to_string()], Some(5))
            .expect("canceled worker should settle");
        server.join().expect("provider server should exit");
        manager.shutdown_and_discard();
    }

    #[test]
    fn timed_out_wait_reports_progress_and_discourages_cancellation() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
        let base_url = format!(
            "http://{}/v1",
            listener.local_addr().expect("test provider address")
        );
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("subagent provider connection");
            read_request(&mut stream).expect("subagent provider request");
            ready_tx.send(()).expect("provider ready signal");
            release_rx.recv().expect("provider release signal");
            write_response(&mut stream, "slow result").expect("subagent provider response");
        });
        let manager = SubagentManager::new("session".into(), 1);
        let id = manager
            .spawn(
                provider_config(base_url),
                "local:model",
                Some("slow"),
                "long work",
                None,
            )
            .expect("slow subagent spawn");
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker should reach the provider");

        let timed_out = manager
            .wait(&[id.to_string()], Some(1))
            .expect("a timed-out wait must still report status");
        assert!(
            timed_out.contains("1 of 1 subagent(s) still running"),
            "the timed-out wait must explain that the run continues; got:\n{timed_out}"
        );
        assert!(
            timed_out.contains("Do not cancel a subagent because a wait timed out"),
            "the timed-out wait must discourage premature cancellation; got:\n{timed_out}"
        );

        release_tx.send(()).expect("release provider response");
        let settled = manager
            .wait(&[id.to_string()], Some(5))
            .expect("released subagent should settle");
        assert!(settled.contains("slow result"));
        assert!(
            !settled.contains("still running"),
            "settled waits must not carry the timeout notice; got:\n{settled}"
        );
        server.join().expect("provider server should exit");
        manager.shutdown_and_discard();
    }

    #[test]
    fn interrupted_wait_explains_that_the_work_continues() {
        crate::set_interrupted(false);
        let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
        let base_url = format!(
            "http://{}/v1",
            listener.local_addr().expect("test provider address")
        );
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("subagent provider connection");
            read_request(&mut stream).expect("subagent provider request");
            ready_tx.send(()).expect("provider ready signal");
            release_rx.recv().expect("provider release signal");
            write_response(&mut stream, "slow result").expect("subagent provider response");
        });
        let manager = SubagentManager::new("session".into(), 1);
        let id = manager
            .spawn(
                provider_config(base_url),
                "local:model",
                Some("slow"),
                "long work",
                None,
            )
            .expect("slow subagent spawn");
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker should reach the provider");

        let token = CancellationToken::default();
        token.cancel();
        crate::cancellation::scope(&token, || {
            let unbounded = manager
                .wait(&[id.to_string()], None)
                .expect("an interrupted wait must still report status");
            assert!(
                unbounded.contains("Wait interrupted"),
                "the interrupted wait must explain that the wait ended; got:\n{unbounded}"
            );
            assert!(
                unbounded.contains("1 of 1 subagent(s) still running"),
                "the interrupted wait must report that the run continues; got:\n{unbounded}"
            );
            assert!(
                unbounded.contains("Do not wait again"),
                "the interrupted wait must discourage immediately waiting again; got:\n{unbounded}"
            );
            assert!(
                !unbounded.contains("Wait ended after"),
                "an interrupted wait must not look like a timeout; got:\n{unbounded}"
            );

            let timed = manager
                .wait(&[id.to_string()], Some(300))
                .expect("interrupt should win over a pending timeout");
            assert!(
                timed.contains("Wait interrupted"),
                "Esc during a timed wait is a cancel, not a timeout; got:\n{timed}"
            );
            assert!(
                !timed.contains("Wait ended after"),
                "interrupt must take precedence over timeout; got:\n{timed}"
            );
        });

        release_tx.send(()).expect("release provider response");
        manager
            .wait(&[id.to_string()], Some(5))
            .expect("released subagent should settle");
        server.join().expect("provider server should exit");
        manager.shutdown_and_discard();
    }

    #[test]
    fn interrupted_wait_stays_quiet_when_every_id_already_settled() {
        crate::set_interrupted(false);
        let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
        let base_url = format!(
            "http://{}/v1",
            listener.local_addr().expect("test provider address")
        );
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("subagent provider connection");
            read_request(&mut stream).expect("subagent provider request");
            write_response(&mut stream, "done").expect("subagent provider response");
        });
        let manager = SubagentManager::new("session".into(), 1);
        let id = manager
            .spawn(
                provider_config(base_url),
                "local:model",
                Some("quick"),
                "short work",
                None,
            )
            .expect("subagent spawn");
        manager
            .wait(&[id.to_string()], Some(5))
            .expect("subagent should settle");

        let token = CancellationToken::default();
        token.cancel();
        crate::cancellation::scope(&token, || {
            let output = manager
                .wait(&[id.to_string()], None)
                .expect("waiting on a settled ID should succeed");
            assert!(
                !output.contains("Wait interrupted"),
                "a finished wait must not claim it was interrupted; got:\n{output}"
            );
            assert!(
                !output.contains("Wait ended after"),
                "a finished wait must not carry a timeout notice; got:\n{output}"
            );
            assert!(output.contains("done"));
        });

        server.join().expect("provider server should exit");
        manager.shutdown_and_discard();
    }

    #[test]
    fn canceling_an_idle_restart_releases_its_capacity() {
        let manager = SubagentManager::new("session".into(), 1);
        {
            let mut state = manager.lock();
            state.active = 1;
            state.entries.push(test_entry(SubagentStatus::Canceling));
        }
        let id = SubagentId::new(1);
        let worker_manager = manager.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = worker_manager.wait_for_work(&id);
            done_tx.send(()).expect("worker completion receiver");
        });

        let returned_promptly = done_rx.recv_timeout(Duration::from_millis(200)).is_ok();
        if !returned_promptly {
            manager.lock().shutting_down = true;
            manager.shared.changed.notify_all();
        }
        worker.join().expect("idle worker should stop");
        let state = manager.lock();
        let active = state.active;
        let status = state.entries[0].snapshot.status;
        let outcome = state.entries[0].snapshot.latest_outcome;
        drop(state);
        manager.shutdown_and_discard();

        assert!(
            returned_promptly,
            "canceling idle workers must not wait for new work"
        );
        assert_eq!(active, 0);
        assert_eq!(status, SubagentStatus::Done);
        assert_eq!(outcome, Some(RunOutcome::Interrupted));
    }

    #[test]
    fn work_canceled_after_dequeue_does_not_enter_the_transcript() {
        let manager = SubagentManager::new("session".into(), 1);
        manager
            .lock()
            .entries
            .push(test_entry(SubagentStatus::Canceling));
        let work = WorkItem {
            message: "canceled follow-up".into(),
            origin: RunOrigin::PrivateUser,
            run_number: 2,
        };

        let started = manager.begin_work(&SubagentId::new(1), &work);
        let state = manager.lock();
        let snapshot = &state.entries[0].snapshot;

        assert!(!started);
        assert_eq!(snapshot.status, SubagentStatus::Done);
        assert_eq!(snapshot.latest_outcome, Some(RunOutcome::Interrupted));
        assert!(snapshot.transcript.is_empty());
    }

    #[test]
    fn settled_agents_reuse_their_conversation_on_restart() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
        let base_url = format!(
            "http://{}/v1",
            listener.local_addr().expect("test provider address")
        );
        let server = std::thread::spawn(move || {
            for response in ["first", "second"] {
                let (mut stream, _) = listener.accept().expect("subagent provider connection");
                read_request(&mut stream).expect("subagent provider request");
                write_response(&mut stream, response).expect("subagent provider response");
            }
        });
        let config = provider_config(base_url);
        let manager = SubagentManager::new("session".into(), 1);
        let id = manager
            .spawn(config, "local:model", Some("reused"), "first task", None)
            .expect("initial subagent spawn");
        manager
            .wait(&[id.to_string()], Some(5))
            .expect("initial subagent run should settle");
        let first_thread = manager
            .lock()
            .entries
            .iter()
            .find(|entry| entry.snapshot.id == id)
            .and_then(|entry| entry.thread_id)
            .expect("settled subagent should retain its worker");
        manager
            .send(id.as_str(), "follow-up", RunOrigin::PrivateUser)
            .expect("settled subagent should restart");
        manager
            .wait(&[id.to_string()], Some(5))
            .expect("restarted subagent should settle");

        let snapshot = manager
            .snapshots()
            .into_iter()
            .find(|snapshot| snapshot.id == id)
            .expect("retained subagent snapshot");
        let second_thread = manager
            .lock()
            .entries
            .iter()
            .find(|entry| entry.snapshot.id == id)
            .and_then(|entry| entry.thread_id)
            .expect("restarted subagent should retain its worker");
        assert_eq!(first_thread, second_thread);
        assert_eq!(snapshot.completed_turns, 2);
        assert!(snapshot.transcript.iter().any(|item| {
            matches!(item.as_ref(), super::super::types::SubagentTranscriptItem::Assistant(text) if text == "first")
        }));
        assert_eq!(snapshot.latest_final_result, "second");
        server.join().expect("provider server should exit");
        manager.shutdown_and_discard();
    }

    #[test]
    fn queued_messages_run_in_order_on_one_worker() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
        let base_url = format!(
            "http://{}/v1",
            listener.local_addr().expect("test provider address")
        );
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            for (index, response) in ["first", "second", "third"].into_iter().enumerate() {
                let (mut stream, _) = listener.accept().expect("subagent provider connection");
                read_request(&mut stream).expect("subagent provider request");
                if index == 0 {
                    ready_tx.send(()).expect("provider ready signal");
                    release_rx.recv().expect("provider release signal");
                }
                write_response(&mut stream, response).expect("subagent provider response");
            }
        });
        let manager = SubagentManager::new("session".into(), 1);
        let id = manager
            .spawn(
                provider_config(base_url),
                "local:model",
                Some("ordered"),
                "first task",
                None,
            )
            .expect("initial subagent spawn");
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("first worker should reach the provider");
        manager
            .send(id.as_str(), "second task", RunOrigin::Model)
            .expect("second task should queue");
        manager
            .send(id.as_str(), "third task", RunOrigin::PrivateUser)
            .expect("third task should queue");
        release_tx.send(()).expect("release provider response");
        manager
            .wait(&[id.to_string()], Some(5))
            .expect("queued work should settle");

        let snapshot = manager
            .snapshots()
            .into_iter()
            .find(|snapshot| snapshot.id == id)
            .expect("ordered subagent snapshot");
        let messages = snapshot
            .transcript
            .iter()
            .filter_map(|item| match item.as_ref() {
                super::super::types::SubagentTranscriptItem::User { text, .. } => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(messages, ["first task", "second task", "third task"]);
        assert_eq!(snapshot.completed_turns, 3);
        assert!(snapshot.queued_messages.is_empty());
        server.join().expect("provider server should exit");
        manager.shutdown_and_discard();
    }

    #[test]
    fn pruning_keeps_active_waited_and_undelivered_entries() {
        let manager = SubagentManager::new("session".into(), 3);
        let mut state = manager.lock();
        for sequence in 1..=MAX_TRACKED_SUBAGENTS as u64 {
            let conversation =
                Conversation::memory(config(), "parent".into(), format!("session-sa-{sequence}"));
            let cancellation = conversation.cancellation_token();
            let mut snapshot = SubagentSnapshot::new(
                SubagentId::new(sequence),
                format!("agent {sequence}"),
                "default".into(),
                "task".into(),
                "parent".into(),
                100,
            );
            snapshot.status = SubagentStatus::Done;
            snapshot.settled_at = Some(
                Instant::now()
                    .checked_sub(Duration::from_secs(MAX_TRACKED_SUBAGENTS as u64 - sequence))
                    .expect("test settlement instant"),
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
                steers: crate::agent::SteerInbox::default(),
            });
        }
        state.deferred.push_back(DeferredResult {
            id: SubagentId::new(1),
            name: "agent 1".into(),
            run_number: 1,
            outcome: RunOutcome::Completed,
            result: "done".into(),
            error: String::new(),
            sequence: 1,
        });
        state.entries[1].wait_interest = 1;
        state.entries[2].snapshot.status = SubagentStatus::Canceling;

        prune_settled(&mut state);

        assert_eq!(state.entries.len(), MAX_TRACKED_SUBAGENTS - 1);
        assert!(
            state
                .entries
                .iter()
                .any(|entry| entry.snapshot.id.as_str() == "sa-1")
        );
        assert!(
            state
                .entries
                .iter()
                .any(|entry| entry.snapshot.id.as_str() == "sa-2")
        );
        assert!(
            state
                .entries
                .iter()
                .any(|entry| entry.snapshot.id.as_str() == "sa-3")
        );
        assert!(
            !state
                .entries
                .iter()
                .any(|entry| entry.snapshot.id.as_str() == "sa-4")
        );
    }

    #[test]
    fn late_wait_consumes_deferred_results_and_private_runs_stay_private() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
        let base_url = format!(
            "http://{}/v1",
            listener.local_addr().expect("test provider address")
        );
        let server = std::thread::spawn(move || {
            for response in ["model result", "private result"] {
                let (mut stream, _) = listener.accept().expect("subagent provider connection");
                read_request(&mut stream).expect("subagent provider request");
                write_response(&mut stream, response).expect("subagent provider response");
            }
        });
        let config = provider_config(base_url);
        let manager = SubagentManager::new("session".into(), 1);
        let id = manager
            .spawn(config, "local:model", Some("delivery"), "model task", None)
            .expect("model-originated run should start");
        let deadline = Instant::now() + Duration::from_secs(5);
        while manager
            .snapshots()
            .iter()
            .any(|snapshot| snapshot.id == id && snapshot.status.is_active())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(manager.has_deferred());

        let waited = manager
            .wait(&[id.to_string()], Some(1))
            .expect("late wait should read the settled result");
        assert!(waited.contains("model result"));
        assert!(!manager.has_deferred());

        manager
            .send(id.as_str(), "private follow-up", RunOrigin::PrivateUser)
            .expect("private takeover should restart the agent");
        manager
            .wait(&[id.to_string()], Some(5))
            .expect("private run should settle");
        assert!(!manager.has_deferred());
        server.join().expect("provider server should exit");
        manager.shutdown_and_discard();
    }

    #[test]
    fn private_cancellation_preserves_an_earlier_model_result() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
        let base_url = format!(
            "http://{}/v1",
            listener.local_addr().expect("test provider address")
        );
        let (first_ready_tx, first_ready_rx) = mpsc::channel();
        let (first_release_tx, first_release_rx) = mpsc::channel();
        let (private_ready_tx, private_ready_rx) = mpsc::channel();
        let (private_release_tx, private_release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().expect("first provider connection");
            read_request(&mut first).expect("first provider request");
            first_ready_tx.send(()).expect("first provider ready");
            first_release_rx.recv().expect("first provider release");
            write_response(&mut first, "model result").expect("first provider response");

            let (mut private, _) = listener.accept().expect("private provider connection");
            read_request(&mut private).expect("private provider request");
            private_ready_tx.send(()).expect("private provider ready");
            private_release_rx.recv().expect("private provider release");
            // Cancellation may already have closed the client connection.
            let _ = write_response(&mut private, "private result");
        });
        let manager = SubagentManager::new("session".into(), 1);
        let id = manager
            .spawn(
                provider_config(base_url),
                "local:model",
                Some("mixed origin"),
                "model task",
                None,
            )
            .expect("model subagent spawn");
        first_ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("model run should reach provider");
        manager
            .send(id.as_str(), "private task", RunOrigin::PrivateUser)
            .expect("private task should queue");
        first_release_tx.send(()).expect("release model response");
        private_ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("private run should reach provider");

        let cancel_manager = manager.clone();
        let cancel_id = id.to_string();
        let cancel = std::thread::spawn(move || cancel_manager.cancel(&[cancel_id], true));
        // Once the wake handler is installed, cancellation may interrupt the
        // provider read and pass through the transient Canceling state before
        // this thread can observe it. Releasing the server remains safe in
        // either ordering.
        private_release_tx
            .send(())
            .expect("release private provider response");
        cancel
            .join()
            .expect("private cancellation thread")
            .expect("private cancellation result");

        let deferred = manager.drain_deferred();
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].result, "model result");
        server.join().expect("provider server should exit");
        manager.shutdown_and_discard();
    }

    #[test]
    fn wait_all_returns_immediately_without_active_subagents() {
        let manager = SubagentManager::new("session".into(), 3);
        assert_eq!(manager.active_count(), 0);
        assert!(manager.wait_all(1));
        manager.shutdown_and_discard();
    }

    #[test]
    fn preset_spawns_apply_the_preset_model_and_agent_label() {
        // Port 9 refuses connections instantly, so the child settles Failed
        // without a server; the assertions are about spawn-time effects.
        let config = provider_config("http://127.0.0.1:9/v1".into());
        let preset = super::super::presets::bundled().remove(0);
        let manager = SubagentManager::new("session".into(), 1);
        let id = manager
            .spawn(config, "local:model", None, "find the bug", Some(&preset))
            .expect("preset spawn");
        manager
            .wait(&[id.to_string()], Some(10))
            .expect("preset child settles");

        let snapshot = manager
            .snapshots()
            .into_iter()
            .find(|snapshot| snapshot.id == id)
            .expect("preset child snapshot");
        assert_eq!(snapshot.agent, "scout");
        assert_eq!(snapshot.model, "local:model", "scout inherits by default");
        manager.shutdown_and_discard();
    }

    #[test]
    fn preset_model_overrides_config() {
        let mut config = provider_config("http://127.0.0.1:9/v1".into());
        config.subagent_model = "local:configured".into();
        let mut preset = super::super::presets::bundled().remove(0);
        preset.model = Some("local:fast".into());

        let via_preset = resolve_model(&config, "local:parent", preset.model.as_deref())
            .expect("preset model applies");
        assert_eq!(via_preset, "local:fast");

        preset.model = None;
        let via_config = resolve_model(&config, "local:parent", preset.model.as_deref())
            .expect("config applies");
        assert_eq!(via_config, "local:configured");
    }

    #[test]
    fn request_budget_steers_then_stops_a_runaway_subagent() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
        let base_url = format!(
            "http://{}/v1",
            listener.local_addr().expect("test provider address")
        );
        let server = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for index in 0..3 {
                let (mut stream, _) = listener.accept().expect("subagent provider connection");
                let body = read_request(&mut stream).expect("subagent provider request");
                bodies.push(body);
                write_tool_call_response(&mut stream, "working", &format!("call-{index}"))
                    .expect("subagent provider response");
            }
            bodies
        });
        let mut config = provider_config(base_url);
        config.subagent_request_budget = 2;
        let manager = SubagentManager::new("session".into(), 1);
        let id = manager
            .spawn(
                config,
                "local:model",
                Some("runaway"),
                "keep working forever",
                None,
            )
            .expect("runaway subagent spawn");
        let waited = manager
            .wait(&[id.to_string()], Some(10))
            .expect("budget-stopped subagent should settle");

        assert!(
            waited.contains("[cancelled after 3 requests]"),
            "the hard stop must deliver a salvage envelope; got:\n{waited}"
        );
        let snapshot = manager
            .snapshots()
            .into_iter()
            .find(|snapshot| snapshot.id == id)
            .expect("runaway subagent snapshot");
        assert_eq!(snapshot.requests, 3);
        assert_eq!(snapshot.run_tokens, 36);
        assert_eq!(manager.total_child_tokens(), 36);
        let bodies = server.join().expect("provider server should exit");
        assert_eq!(bodies.len(), 3, "the fourth request must never happen");
        assert!(
            bodies[2].contains("Request budget reached"),
            "the steer message must ride the final request; got:\n{}",
            bodies[2]
        );
        manager.shutdown_and_discard();
    }

    #[test]
    fn run_timeout_interrupts_an_in_flight_subagent_request() {
        crate::install_interrupt_handler().expect("interrupt handler");
        let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
        let base_url = format!(
            "http://{}/v1",
            listener.local_addr().expect("test provider address")
        );
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("subagent provider connection");
            read_request(&mut stream).expect("subagent provider request");
            std::thread::sleep(Duration::from_secs(3));
            // The timeout should close or interrupt the client before this
            // response. A broken pipe is therefore an expected outcome.
            let _ = write_tool_call_response(&mut stream, "too late", "call-0");
        });
        let mut config = provider_config(base_url);
        config.subagent_timeout_secs = 1;
        let manager = SubagentManager::new("session".into(), 1);
        let started = Instant::now();
        let id = manager
            .spawn(config, "local:model", Some("slow"), "slow work", None)
            .expect("slow subagent spawn");
        let waited = manager
            .wait(&[id.to_string()], Some(10))
            .expect("timed-out subagent should settle");

        assert!(
            waited.contains("[cancelled after"),
            "the timeout stop must deliver a salvage marker; got:\n{waited}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the watchdog must interrupt the in-flight provider request"
        );
        server.join().expect("provider server should exit");
        manager.shutdown_and_discard();
    }

    #[test]
    fn salvage_envelope_reports_requests_and_last_activity() {
        let mut snapshot = SubagentSnapshot::new(
            SubagentId::new(1),
            "agent".into(),
            "default".into(),
            "prompt".into(),
            "local:model".into(),
            100,
        );
        snapshot.requests = 4;
        assert_eq!(
            salvage_result(&snapshot, ""),
            "[cancelled after 4 requests]",
            "an empty transcript delivers the bare marker"
        );
        snapshot.push_transcript(SubagentTranscriptItem::Assistant("found the bug".into()));
        assert_eq!(
            salvage_result(&snapshot, ""),
            "[cancelled after 4 requests — last activity: \"found the bug\"]"
        );
        assert_eq!(
            salvage_result(&snapshot, "full result"),
            "[cancelled after 4 requests]\nfull result"
        );
    }
}
