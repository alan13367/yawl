//! Subagent capacity, execution, waiting, and delivery facade.
//!
//! The manager rescans capacity and worker lifecycles from one shared state
//! guarded by a mutex. Children own admission and queueing (`capacity`),
//! worker threads (`execution`), blocking observers (`waiting`), deferred
//! result delivery (`delivery`), input validation (`validation`), and
//! presentation (`format`).

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

use crate::cancellation::CancellationToken;
use crate::provider::UsageSummary;

use super::types::{RunOrigin, RunOutcome, SubagentId, SubagentSnapshot};

mod capacity;
mod delivery;
mod execution;
mod format;
mod validation;
mod waiting;

#[cfg(test)]
mod tests;

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
    steer_origins: VecDeque<RunOrigin>,
    /// An accepted orchestrator steer makes this turn eligible for delivery.
    model_steer_accepted: bool,
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

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.shared.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}
