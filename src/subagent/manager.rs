use std::collections::{HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::agent::Conversation;
use crate::cancellation::CancellationToken;
use crate::config::Config;

use super::types::{
    MAX_ERROR_BYTES, MAX_FINAL_RESULT_BYTES, MAX_PROMPT_CHARS, MAX_QUEUE_MESSAGES,
    MAX_TRACKED_SUBAGENTS, QueuedSubagentMessage, RunOrigin, RunOutcome, SubagentId,
    SubagentSnapshot, SubagentStatus, bounded,
};

const DEFAULT_WAIT_SECS: u64 = 30;
const MAX_WAIT_SECS: u64 = 300;
const CANCEL_WAIT: Duration = Duration::from_secs(5);

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
}

struct Entry {
    snapshot: SubagentSnapshot,
    conversation: Option<Conversation>,
    cancellation: CancellationToken,
    work: VecDeque<WorkItem>,
    next_run_number: u64,
    thread_id: Option<usize>,
    handle: Option<JoinHandle<()>>,
    wait_interest: usize,
    pending_delivery: Vec<PendingDelivery>,
    suppress_delivery: bool,
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
        name: &str,
        prompt: &str,
        requested_model: Option<&str>,
    ) -> Result<SubagentId, String> {
        let name = validate_name(name)?;
        let prompt = validate_message(prompt, "prompt")?;
        let model = resolve_model(&config, parent_model, requested_model)?;

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
            let synthetic_session = format!("{}-{id}", state.session_id);
            let conversation =
                Conversation::memory(config.clone(), model.clone(), synthetic_session);
            let cancellation = conversation.cancellation_token();
            let work = WorkItem {
                message: prompt.clone(),
                origin: RunOrigin::Model,
                run_number: 1,
            };
            let snapshot = SubagentSnapshot::new(
                id.clone(),
                name,
                prompt,
                model.clone(),
                crate::model::context_window(&config, &model),
            );
            state.active = state.active.saturating_add(1);
            state.entries.push(Entry {
                snapshot,
                conversation: None,
                cancellation,
                work: VecDeque::new(),
                next_run_number: 2,
                thread_id: None,
                handle: None,
                wait_interest: 0,
                pending_delivery: Vec::new(),
                suppress_delivery: false,
            });
            (id, conversation, work)
        };
        self.start_worker(id.clone(), conversation, work)?;
        Ok(id)
    }

    pub(crate) fn send(
        &self,
        id: &str,
        message: &str,
        origin: RunOrigin,
    ) -> Result<String, String> {
        let message = validate_message(message, "message")?;
        let (id, conversation, work, old_handle) = {
            let mut state = self.lock();
            let index = find_index(&state, id)?;
            let is_active = state.entries[index].snapshot.status.is_active();
            if is_active {
                let entry = &mut state.entries[index];
                if entry.snapshot.status == SubagentStatus::Canceling {
                    return Err(format!("{id} is canceling"));
                }
                if entry.work.len() >= MAX_QUEUE_MESSAGES {
                    return Err(format!(
                        "{id} already has {MAX_QUEUE_MESSAGES} queued messages"
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
            let conversation = entry
                .conversation
                .take()
                .ok_or_else(|| format!("{id} has no retained conversation"))?;
            let run_number = entry.next_run_number;
            entry.next_run_number = entry.next_run_number.saturating_add(1);
            let work = WorkItem {
                message,
                origin,
                run_number,
            };
            entry.snapshot.status = SubagentStatus::Starting;
            entry.snapshot.current_activity = "starting".into();
            entry.snapshot.settled_at = None;
            entry.cancellation.clear();
            entry.suppress_delivery = false;
            let old_handle = entry.handle.take();
            let id = entry.snapshot.id.clone();
            state.active = state.active.saturating_add(1);
            (id, conversation, work, old_handle)
        };
        if let Some(handle) = old_handle {
            let _ = handle.join();
        }
        self.start_worker(id.clone(), conversation, work)?;
        Ok(format!("restarted {id}"))
    }

    pub(crate) fn wait(&self, ids: &[String], timeout_secs: Option<u64>) -> Result<String, String> {
        validate_id_list(ids)?;
        let timeout_secs = timeout_secs.unwrap_or(DEFAULT_WAIT_SECS);
        if !(1..=MAX_WAIT_SECS).contains(&timeout_secs) {
            return Err(format!(
                "timeout_secs must be between 1 and {MAX_WAIT_SECS}"
            ));
        }
        let timeout = Duration::from_secs(timeout_secs);
        let deadline = Instant::now().checked_add(timeout);
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

    pub(crate) fn snapshots(&self) -> Vec<SubagentSnapshot> {
        self.lock()
            .entries
            .iter()
            .map(|entry| entry.snapshot.clone())
            .collect()
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
        conversation: Conversation,
        first_work: WorkItem,
    ) -> Result<(), String> {
        let holder = Arc::new(Mutex::new(Some(conversation)));
        let worker_holder = Arc::clone(&holder);
        let manager = self.clone();
        let worker_id = id.clone();
        let spawn = std::thread::Builder::new()
            .name(format!("yawl-{id}"))
            .spawn(move || {
                let mut conversation = lock_mutex(&worker_holder)
                    .take()
                    .expect("subagent conversation is installed before spawning");
                manager.worker_started(&worker_id);
                let result = catch_unwind(AssertUnwindSafe(|| {
                    manager.worker_loop(&worker_id, &mut conversation, first_work)
                }));
                manager.worker_finished(worker_id, conversation, result);
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
                let conversation = lock_mutex(&holder).take();
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
                    entry.conversation = conversation;
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

    fn worker_loop(
        &self,
        id: &SubagentId,
        conversation: &mut Conversation,
        mut work: WorkItem,
    ) -> RunOutcome {
        loop {
            {
                let mut state = self.lock();
                let Some(entry) = state
                    .entries
                    .iter_mut()
                    .find(|entry| entry.snapshot.id == *id)
                else {
                    return RunOutcome::Interrupted;
                };
                if entry.snapshot.status == SubagentStatus::Canceling {
                    entry.snapshot.latest_outcome = Some(RunOutcome::Interrupted);
                    return RunOutcome::Interrupted;
                }
                entry
                    .snapshot
                    .begin_turn(&work.message, work.origin, work.run_number);
            }
            let result = conversation.run_turn_preserving_cancellation(
                Some(work.message.clone()),
                &mut |event| {
                    let mut state = self.lock();
                    if let Some(entry) = state
                        .entries
                        .iter_mut()
                        .find(|entry| entry.snapshot.id == *id)
                    {
                        entry.snapshot.apply_event(event);
                    }
                },
            );
            let (outcome, error) = match result {
                Ok(true) => (RunOutcome::Completed, None),
                Ok(false) | Err(crate::error::Error::Interrupted) => {
                    (RunOutcome::Interrupted, None)
                }
                Err(error) => (RunOutcome::Failed, Some(error.to_string())),
            };
            let final_result = conversation.latest_turn_result();
            let mut state = self.lock();
            let Some(entry) = state
                .entries
                .iter_mut()
                .find(|entry| entry.snapshot.id == *id)
            else {
                return outcome;
            };
            entry
                .snapshot
                .finish_turn(outcome, &final_result, error.as_deref());
            if work.origin == RunOrigin::Model && !entry.suppress_delivery {
                entry.pending_delivery.push(PendingDelivery {
                    run_number: work.run_number,
                    outcome,
                    result: bounded(&final_result, MAX_FINAL_RESULT_BYTES),
                    error: error
                        .as_deref()
                        .map_or_else(String::new, |text| bounded(text, MAX_ERROR_BYTES)),
                });
            }
            self.shared.changed.notify_all();
            if entry.snapshot.status == SubagentStatus::Canceling {
                return RunOutcome::Interrupted;
            }
            let Some(next) = entry.work.pop_front() else {
                return outcome;
            };
            if !entry.snapshot.queued_messages.is_empty() {
                entry.snapshot.queued_messages.remove(0);
            }
            entry.cancellation.clear();
            work = next;
        }
    }

    fn worker_finished(
        &self,
        id: SubagentId,
        conversation: Conversation,
        worker_result: std::thread::Result<RunOutcome>,
    ) {
        let mut state = self.lock();
        let Some(index) = state
            .entries
            .iter()
            .position(|entry| entry.snapshot.id == id)
        else {
            return;
        };
        let now = Instant::now();
        let shutting_down = state.shutting_down;
        let mut pending = Vec::new();
        {
            let entry = &mut state.entries[index];
            entry.thread_id = None;
            entry.conversation = Some(conversation);
            entry.snapshot.settled_at = Some(now);
            entry.snapshot.current_tool = None;
            entry.snapshot.live_assistant.clear();
            entry.snapshot.live_reasoning.clear();
            entry.snapshot.current_activity.clear();
            match worker_result {
                Ok(RunOutcome::Failed) => entry.snapshot.status = SubagentStatus::Failed,
                Ok(outcome) => {
                    entry.snapshot.status = SubagentStatus::Done;
                    entry.snapshot.latest_outcome.get_or_insert(outcome);
                }
                Err(_) => {
                    entry.snapshot.status = SubagentStatus::Failed;
                    entry.snapshot.error = "subagent worker panicked".into();
                    entry.snapshot.latest_outcome = Some(RunOutcome::Failed);
                    if entry.snapshot.origin == RunOrigin::Model
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
                }
            }
            // Completed runs always defer their full results; a concurrent
            // wait drains them, and otherwise they arrive as background
            // follow-up delivery.
            if shutting_down {
                entry.pending_delivery.clear();
            } else {
                pending = std::mem::take(&mut entry.pending_delivery);
            }
        }
        state.active = state.active.saturating_sub(1);
        for delivery in pending {
            state.settlement_sequence = state.settlement_sequence.saturating_add(1);
            let snapshot = &state.entries[index].snapshot;
            let deferred = DeferredResult {
                id: snapshot.id.clone(),
                name: snapshot.name.clone(),
                run_number: delivery.run_number,
                outcome: delivery.outcome,
                result: delivery.result,
                error: delivery.error,
                sequence: state.settlement_sequence,
            };
            state.deferred.push_back(deferred);
        }
        self.shared.changed.notify_all();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.shared.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(value) => value,
        Err(poisoned) => poisoned.into_inner(),
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
    requested_model: Option<&str>,
) -> Result<String, String> {
    if let Some(model) = requested_model {
        let model = model.trim();
        if model.is_empty() {
            return Err("model must not be empty".into());
        }
        return Ok(model.to_string());
    }
    if config.subagent_model != "inherit" {
        return Ok(config.subagent_model.clone());
    }
    Ok(parent_model.to_string())
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
        "{} [{}] {}\nmodel: {}\ninitial task: {}\ncontext: {}% ({}/{})\nelapsed: {}\nactivity: {}\noutcome: {}\nturns: {}\nqueued: {}",
        snapshot.id,
        snapshot.status.label(),
        snapshot.name,
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
    use std::path::PathBuf;
    use std::sync::mpsc;

    use super::*;
    use crate::config::{DEFAULT_MAX_TOKENS, DEFAULT_SUBAGENT_MODEL, ProviderConfig, UiColor};

    fn config() -> Config {
        Config {
            model: Some("parent".into()),
            anthropic_base_url: String::new(),
            openai_base_url: String::new(),
            max_tokens: DEFAULT_MAX_TOKENS,
            reasoning_effort: None,
            hide_reasoning: false,
            accent_color: UiColor::WHITE,
            scroll_bar: true,
            context_windows: HashMap::new(),
            auto_compact: false,
            compact_threshold: 0.85,
            subagents: true,
            max_subagents: 3,
            subagent_model: DEFAULT_SUBAGENT_MODEL.into(),
            skill_dirs: Vec::new(),
            providers: HashMap::<String, ProviderConfig>::new(),
            home_dir: PathBuf::new(),
            project_dir: PathBuf::new(),
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

    fn read_request(stream: &mut TcpStream) -> std::io::Result<()> {
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        let mut wanted = None;
        loop {
            let read = stream.read(&mut buffer)?;
            if read == 0 {
                return Ok(());
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
                return Ok(());
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
                "scanner",
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
                "reused",
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
    fn model_precedence_prefers_spawn_then_config_then_parent() {
        let mut config = config();
        assert_eq!(
            resolve_model(&config, "parent", Some("spawn")).expect("spawn model"),
            "spawn"
        );
        config.subagent_model = "configured".into();
        assert_eq!(
            resolve_model(&config, "parent", None).expect("configured model"),
            "configured"
        );
        config.subagent_model = "inherit".into();
        assert_eq!(
            resolve_model(&config, "parent", None).expect("inherited model"),
            "parent"
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
    fn wait_format_reports_every_id_with_complete_results() {
        let snapshots = (1..=MAX_TRACKED_SUBAGENTS as u64)
            .map(|sequence| {
                let mut snapshot = SubagentSnapshot::new(
                    SubagentId::new(sequence),
                    format!("agent {sequence}"),
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
            .spawn(config.clone(), "local:model", "first", "work", None)
            .expect("first spawn should reserve the only slot");
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("first worker should reach the provider");

        let error = manager
            .spawn(config, "local:model", "second", "work", None)
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
            .spawn(config, "local:model", "reused", "first task", None)
            .expect("initial subagent spawn");
        manager
            .wait(&[id.to_string()], Some(5))
            .expect("initial subagent run should settle");
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
        assert_eq!(snapshot.completed_turns, 2);
        assert!(snapshot.transcript.iter().any(|item| {
            matches!(item, super::super::types::SubagentTranscriptItem::Assistant(text) if text == "first")
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
                "ordered",
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
            .filter_map(|item| match item {
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
                conversation: Some(conversation),
                cancellation,
                work: VecDeque::new(),
                next_run_number: 2,
                thread_id: None,
                handle: None,
                wait_interest: 0,
                pending_delivery: Vec::new(),
                suppress_delivery: false,
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
            .spawn(config, "local:model", "delivery", "model task", None)
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
            write_response(&mut private, "private result").expect("private provider response");
        });
        let manager = SubagentManager::new("session".into(), 1);
        let id = manager
            .spawn(
                provider_config(base_url),
                "local:model",
                "mixed origin",
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
        let deadline = Instant::now() + Duration::from_secs(1);
        while !manager
            .snapshots()
            .iter()
            .any(|snapshot| snapshot.id == id && snapshot.status == SubagentStatus::Canceling)
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            manager.snapshots().iter().any(|snapshot| {
                snapshot.id == id && snapshot.status == SubagentStatus::Canceling
            })
        );
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
}
