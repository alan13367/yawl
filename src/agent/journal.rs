use crate::error::Error;
use crate::provider::Message;
use crate::session::Session;

/// Conversation persistence that becomes a no-op for memory-only subagents.
pub(super) struct Journal {
    id: String,
    persistent: Option<Session>,
}

impl Journal {
    pub(super) fn persistent(session: Session) -> Self {
        Self {
            id: session.id.clone(),
            persistent: Some(session),
        }
    }

    pub(super) fn memory(id: String) -> Self {
        Self {
            id,
            persistent: None,
        }
    }

    pub(super) fn id(&self) -> &str {
        &self.id
    }

    pub(super) fn append_message(&mut self, message: &Message) -> Result<(), Error> {
        match self.persistent.as_mut() {
            Some(session) => session.append_message(message),
            None => Ok(()),
        }
    }

    pub(super) fn append_compaction_range(
        &mut self,
        summary: &str,
        start: usize,
        replaced: usize,
    ) -> Result<(), Error> {
        match self.persistent.as_mut() {
            Some(session) => session.append_compaction_range(summary, start, replaced),
            None => Ok(()),
        }
    }

    pub(super) fn append_undo_event(
        &mut self,
        dropped: usize,
        clear_goal: bool,
    ) -> Result<(), Error> {
        match self.persistent.as_mut() {
            Some(session) => session.append_undo_event(dropped, clear_goal),
            None => Ok(()),
        }
    }

    pub(super) fn append_goal_start(&mut self, goal: &str, message: &Message) -> Result<(), Error> {
        match self.persistent.as_mut() {
            Some(session) => session.append_goal_start(goal, message),
            None => Ok(()),
        }
    }

    pub(super) fn append_goal_complete(&mut self, message: &Message) -> Result<(), Error> {
        match self.persistent.as_mut() {
            Some(session) => session.append_goal_complete(message),
            None => Ok(()),
        }
    }

    pub(super) fn append_goal_cancel(&mut self) -> Result<(), Error> {
        match self.persistent.as_mut() {
            Some(session) => session.append_goal_cancel(),
            None => Ok(()),
        }
    }
}
