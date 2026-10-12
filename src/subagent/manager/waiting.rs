//! Blocking observers and control: wait, wait_all, cancel, shutdown.

use std::time::{Duration, Instant};

use crate::provider::UsageSummary;
use crate::subagent::types::{SubagentSnapshot, SubagentStatus};

use super::format::{
    format_deferred_result, format_detailed_snapshot, format_duration, format_snapshots,
};
use super::validation::{find_index, resolve_indexes, validate_id_list, wait_timeout};
use super::{Entry, State, SubagentManager};
use crate::subagent::types::SubagentId;
use std::sync::MutexGuard;

const CANCEL_WAIT: Duration = Duration::from_secs(5);

/// Registers wait interest in the entries at `indexes`, so their results are
/// reported to the waiter instead of deferred, and returns their IDs.
fn watch(state: &mut State, indexes: &[usize]) -> Vec<SubagentId> {
    indexes
        .iter()
        .map(|index| {
            let entry = &mut state.entries[*index];
            entry.wait_interest = entry.wait_interest.saturating_add(1);
            entry.snapshot.id.clone()
        })
        .collect()
}

impl Entry {
    /// Stops an active run: drops its queued work and messages, cancels its
    /// token, and returns the worker thread to wake.
    fn begin_cancel(&mut self) -> Option<usize> {
        self.snapshot.status = SubagentStatus::Canceling;
        self.snapshot.current_activity = "canceling".into();
        self.work.clear();
        self.snapshot.queued_messages.clear();
        self.cancellation.cancel();
        self.thread_id
    }
}

impl SubagentManager {
    /// Blocks until every watched run settles, the turn is interrupted, or
    /// `deadline` passes, then releases the wait interest from [`watch`] and
    /// returns the runs' latest snapshots.
    fn settle<'a>(
        &'a self,
        mut state: MutexGuard<'a, State>,
        selected: Vec<SubagentId>,
        deadline: Option<Instant>,
    ) -> (MutexGuard<'a, State>, Vec<SubagentSnapshot>) {
        while !(selected.iter().all(|id| {
            state
                .index_of(id)
                .is_none_or(|index| !state.entries[index].snapshot.status.is_active())
        }) || crate::cancellation::interrupted()
            || deadline.is_some_and(|deadline| Instant::now() >= deadline))
        {
            let remaining = deadline
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::from_millis(100))
                .min(Duration::from_millis(100));
            state = match self.shared.changed.wait_timeout(state, remaining) {
                Ok((state, _)) => state,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        let mut snapshots = Vec::with_capacity(selected.len());
        for id in &selected {
            if let Some(entry) = state.entry_mut(id) {
                entry.wait_interest = entry.wait_interest.saturating_sub(1);
                snapshots.push(entry.snapshot.clone());
            }
        }
        (state, snapshots)
    }

    pub(crate) fn wait(&self, ids: &[String], timeout_secs: Option<u64>) -> Result<String, String> {
        validate_id_list(ids)?;
        let timeout = wait_timeout(timeout_secs)?;
        let deadline = timeout.and_then(|timeout| Instant::now().checked_add(timeout));
        let mut state = self.lock();
        let indexes = resolve_indexes(&state, ids)?;
        let selected = watch(&mut state, &indexes);
        let (mut state, snapshots) = self.settle(state, selected, deadline);
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
        // An interrupt that also stopped the children (the TUI's Ctrl+C)
        // leaves them canceling; their results are suppressed, not deferred.
        let canceling = snapshots
            .iter()
            .filter(|snapshot| snapshot.status == SubagentStatus::Canceling)
            .count();
        if active > 0 {
            if crate::cancellation::interrupted() && canceling == active {
                output = format!(
                    "Wait interrupted: the user stopped this turn and {active} of {} \
                     subagent(s) with it. Their partial work is below and no further results \
                     will arrive. Do not restart them unless the user asks.\n\n{output}",
                    snapshots.len()
                );
            } else if crate::cancellation::interrupted() {
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
                     running. This is normal: subagent tasks often take 10+ minutes. Call \
                     subagent_wait again without timeout_secs to block until they finish, and do \
                     not redo their work meanwhile. Do not cancel a subagent because a wait timed \
                     out.\n\n{output}",
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
        let selected = watch(&mut state, &indexes);
        let mut wake_threads = Vec::new();
        for index in &indexes {
            let entry = &mut state.entries[*index];
            if entry.snapshot.status.is_active() {
                wake_threads.extend(entry.begin_cancel());
                if private {
                    entry.suppress_delivery = true;
                }
            }
        }
        // Workers queued for a slot block on the condition variable rather
        // than in I/O, so they need a notification to observe the cancel.
        self.shared.changed.notify_all();
        if !private {
            state
                .deferred
                .retain(|delivery| !ids.iter().any(|id| id == delivery.id.as_str()));
        }
        for thread in wake_threads {
            crate::cancellation::wake_thread(thread);
        }
        let (_state, snapshots) = self.settle(state, selected, deadline);
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
                    threads.extend(entry.begin_cancel());
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
        state.running = 0;
        self.shared.changed.notify_all();
    }

    pub(crate) fn interrupt_all(&self) {
        let threads = {
            let mut state = self.lock();
            let mut threads = Vec::new();
            for entry in &mut state.entries {
                if entry.snapshot.status.is_active() {
                    entry.suppress_delivery = true;
                    threads.extend(entry.begin_cancel());
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
