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

    pub(super) fn append_compaction(
        &mut self,
        summary: &str,
        replaced: usize,
    ) -> Result<(), Error> {
        match self.persistent.as_mut() {
            Some(session) => session.append_compaction(summary, replaced),
            None => Ok(()),
        }
    }

    pub(super) fn append_undo(&mut self, dropped: usize) -> Result<(), Error> {
        match self.persistent.as_mut() {
            Some(session) => session.append_undo(dropped),
            None => Ok(()),
        }
    }
}
