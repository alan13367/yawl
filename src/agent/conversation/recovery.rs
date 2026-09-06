use super::{Conversation, ConversationKind, UndoReport};
use crate::error::Error;
use crate::provider::Message;

impl Conversation {
    /// Keep a completed tool's output until its append succeeds. Never execute
    /// another tool while an earlier result is waiting for storage.
    pub(super) fn save_tool_result(&mut self, result: Message) -> Result<(), Error> {
        self.pending_tool_results.push_back(result);
        self.flush_tool_results()
    }

    fn flush_tool_results(&mut self) -> Result<(), Error> {
        while let Some(result) = self.pending_tool_results.pop_front() {
            if let Err(error) = self.persist_message(&result) {
                self.pending_tool_results.push_front(result);
                return Err(error);
            }
            self.messages.push(result);
        }
        Ok(())
    }

    pub(super) fn recover_history(&mut self) -> Result<(), Error> {
        self.flush_tool_results()?;
        self.recover_undo()?;
        let previous_len = self.messages.len();
        match &mut self.kind {
            ConversationKind::Persistent(state) => {
                state.session.repair_tool_results(&mut self.messages)?
            }
            ConversationKind::Child(_) => {
                while let Some((index, results)) =
                    crate::session::missing_tool_results(&self.messages)
                {
                    self.messages.splice(index..index, results);
                }
            }
        }
        if self.messages.len() != previous_len {
            self.context_tokens = 0;
            self.context_usage = None;
        }
        Ok(())
    }

    pub(super) fn recover_undo(&mut self) -> Result<Option<UndoReport>, Error> {
        let ConversationKind::Persistent(state) = &mut self.kind else {
            return Ok(None);
        };
        let Some(undo) = state.session.pending_undo().cloned() else {
            return Ok(None);
        };
        let restore = if undo.committed {
            crate::checkpoint::RestoreReport {
                restored: undo.checkpoint.is_some(),
                ..Default::default()
            }
        } else {
            let restore = state.checkpoints.restore_retained(undo.checkpoint)?;
            state.session.append_undo_event_with_plan(
                undo.dropped,
                undo.clear_goal,
                undo.plan_state,
            )?;
            self.messages
                .truncate(self.messages.len().saturating_sub(undo.dropped));
            state.active_goal = state.session.active_goal().map(str::to_string);
            self.context_tokens = 0;
            self.context_usage = None;
            self.latest_turn_result.clear();
            self.plan_ready_this_turn = false;
            restore
        };
        state.checkpoints.discard_restored(undo.checkpoint)?;
        state.session.finish_undo()?;
        Ok(Some(UndoReport {
            dropped: undo.dropped,
            restored_files: restore.restored,
            reset_head: restore.reset_head,
            warning: restore.warning,
        }))
    }
}
