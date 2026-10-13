use super::{Conversation, UndoReport};
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
        while let Some(result) = self.pending_tool_results.front() {
            self.session.append_message(result.clone())?;
            self.pending_tool_results.pop_front();
        }
        Ok(())
    }

    pub(super) fn recover_history(&mut self) -> Result<(), Error> {
        self.flush_tool_results()?;
        self.recover_undo()?;
        if self.session.repair_tool_results()? {
            self.context_tokens = 0;
            self.context_usage = None;
        }
        Ok(())
    }

    pub(super) fn recover_undo(&mut self) -> Result<Option<UndoReport>, Error> {
        let Some(undo) = self.session.pending_undo().cloned() else {
            return Ok(None);
        };
        let restore = if undo.committed {
            crate::checkpoint::RestoreReport {
                restored: undo.checkpoint.is_some(),
                ..Default::default()
            }
        } else {
            let restore = match &self.services.checkpoints {
                Some(checkpoints) => checkpoints.restore_retained(undo.checkpoint)?,
                None => crate::checkpoint::RestoreReport::default(),
            };
            self.session
                .append_undo(undo.dropped, undo.clear_goal, undo.plan_state)?;
            self.context_tokens = 0;
            self.context_usage = None;
            self.latest_turn_result.clear();
            self.plan_ready_this_turn = false;
            self.unfinished_mode = None;
            restore
        };
        if let Some(checkpoints) = &mut self.services.checkpoints {
            checkpoints.discard_restored(undo.checkpoint)?;
        }
        self.session.finish_undo()?;
        Ok(Some(UndoReport {
            dropped: undo.dropped,
            restored_files: restore.restored,
            reset_head: restore.reset_head,
            warning: restore.warning,
        }))
    }
}
