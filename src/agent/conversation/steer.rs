//! Shared inbox for mid-turn steering from the TUI worker thread.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::provider::TurnInput;

#[derive(Clone, Default)]
pub(crate) struct SteerInbox {
    pending: Arc<Mutex<VecDeque<TurnInput>>>,
    reasoning_effort: Arc<Mutex<Option<Option<String>>>>,
}

impl SteerInbox {
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<TurnInput>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn push(&self, input: TurnInput) {
        self.lock().push_back(input);
    }

    pub(crate) fn set_reasoning_effort(&self, effort: Option<String>) {
        *self
            .reasoning_effort
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(effort);
    }

    pub(crate) fn take_reasoning_effort(&self) -> Option<Option<String>> {
        self.reasoning_effort
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.lock().is_empty()
    }

    pub(crate) fn drain(&self) -> Vec<TurnInput> {
        self.lock().drain(..).collect()
    }

    pub(crate) fn prepend(&self, inputs: impl IntoIterator<Item = TurnInput>) {
        let inputs = inputs.into_iter().collect::<Vec<_>>();
        let mut pending = self.lock();
        for input in inputs.into_iter().rev() {
            pending.push_front(input);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_reasoning_change_is_taken_at_the_next_request_boundary() {
        let ui = SteerInbox::default();
        let worker = ui.clone();
        assert_eq!(worker.take_reasoning_effort(), None);
        ui.set_reasoning_effort(Some("low".into()));
        ui.set_reasoning_effort(Some("high".into()));
        assert_eq!(worker.take_reasoning_effort(), Some(Some("high".into())));
        ui.set_reasoning_effort(None);
        assert_eq!(worker.take_reasoning_effort(), Some(None));
        assert_eq!(worker.take_reasoning_effort(), None);
    }
}
