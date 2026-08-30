//! Shared inbox for mid-turn steering from the TUI worker thread.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::provider::TurnInput;

#[derive(Clone, Default)]
pub(crate) struct SteerInbox {
    pending: Arc<Mutex<VecDeque<TurnInput>>>,
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
