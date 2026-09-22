//! Blocking observers and control: wait, wait_all, cancel, shutdown.

use std::time::{Duration, Instant};

use crate::provider::UsageSummary;
use crate::subagent::types::{SubagentSnapshot, SubagentStatus};

use super::SubagentManager;
use super::format::{
    format_deferred_result, format_detailed_snapshot, format_duration, format_snapshots,
};
use super::validation::{find_index, resolve_indexes, validate_id_list, wait_timeout};

const CANCEL_WAIT: Duration = Duration::from_secs(5);

impl SubagentManager {
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

    pub(crate) fn total_child_tokens(&self) -> u64 {
        self.lock().total_child_usage.tokens.total_tokens()
    }

    pub(crate) fn total_child_usage(&self) -> UsageSummary {
        self.lock().total_child_usage
    }

    pub(crate) fn active_count(&self) -> usize {
        self.lock().active
    }

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
}
