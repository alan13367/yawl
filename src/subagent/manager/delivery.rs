//! Deferred result delivery: settlement handoff to the orchestrator.

#[cfg(test)]
use crate::subagent::types::{RunOutcome, SubagentId};

use super::{DeferredResult, State, SubagentManager};

impl SubagentManager {
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
}

pub(super) fn flush_pending_deliveries(state: &mut State, index: usize) {
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
