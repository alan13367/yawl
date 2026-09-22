//! Subagent worker threads: lifecycle, turn execution, and settlement.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Instant;

use crate::agent::Conversation;
use crate::subagent::reports;
use crate::subagent::types::{
    MAX_ERROR_BYTES, MAX_FINAL_RESULT_BYTES, MAX_QUEUE_MESSAGES, QueuedSubagentMessage, RunOrigin,
    RunOutcome, SubagentId, SubagentStatus, bounded,
};

use super::delivery::flush_pending_deliveries;
use super::format::salvage_result;
use super::{PendingDelivery, Shared, State, SubagentManager, WorkItem};

impl SubagentManager {
    pub(super) fn start_worker(
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
                        crate::subagent::types::MAX_ERROR_BYTES,
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
                Ok(false) | Err(crate::error::Error::Interrupted) => (
                    RunOutcome::Interrupted,
                    conversation
                        .run_stop_reason()
                        .map(|reason| reason.message().to_string()),
                ),
                // Attribution: a failed child rarely knows which provider or
                // model actually errored, so the label rides the error text.
                Err(error) => (RunOutcome::Failed, Some(format!("[{run_model}] {error}"))),
            };
            let mut leftovers = conversation.take_unaccepted_steers();
            // Persist outside the manager lock so disk I/O does not block
            // cancellation, steering, or other workers' progress events.
            let final_result = reports::prepare(
                &conversation.config().home_dir.join("artifacts/subagents"),
                &conversation.latest_turn_result(),
            );
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
            let canceling = entry.snapshot.status == SubagentStatus::Canceling;
            if canceling {
                leftovers.clear();
                entry.work.clear();
                entry.snapshot.queued_messages.clear();
            }
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

    pub(super) fn wait_for_work(&self, id: &SubagentId) -> Option<WorkItem> {
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

    pub(super) fn begin_work(&self, id: &SubagentId, work: &WorkItem) -> bool {
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
}

pub(super) fn apply_turn_event(
    state: &mut State,
    id: &SubagentId,
    event: crate::agent::TurnEvent<'_>,
) {
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
