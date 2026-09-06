use serde::{Deserialize, Serialize};

use super::{ContextUsage, PlanState, Session, SessionEvent};
use crate::error::Error;
use crate::provider::{Message, Role};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingUndo {
    pub dropped: usize,
    pub clear_goal: bool,
    pub plan_state: Option<PlanState>,
    pub checkpoint: Option<usize>,
    pub committed: bool,
}

impl Session {
    pub(crate) fn pending_undo(&self) -> Option<&PendingUndo> {
        self.pending_undo.as_deref()
    }

    pub(crate) fn begin_undo(&mut self, undo: PendingUndo) -> Result<(), Error> {
        if self.pending_undo.is_some() {
            return Err(Error::Protocol("another undo is unfinished".into()));
        }
        self.append(&SessionEvent::UndoStarted { undo: undo.clone() })?;
        self.pending_undo = Some(Box::new(undo));
        Ok(())
    }

    pub(crate) fn finish_undo(&mut self) -> Result<(), Error> {
        self.append(&SessionEvent::UndoFinished)?;
        self.pending_undo = None;
        Ok(())
    }

    pub(crate) fn context(&self) -> Option<&ContextUsage> {
        self.context.as_ref()
    }

    pub(crate) fn record_context(&mut self, context: ContextUsage) -> Result<(), Error> {
        self.append(&SessionEvent::Context {
            context: context.clone(),
        })?;
        self.context = Some(context);
        Ok(())
    }

    pub(crate) fn repair_tool_results(&mut self, messages: &mut Vec<Message>) -> Result<(), Error> {
        while let Some((index, results)) = missing_tool_results(messages) {
            self.append(&SessionEvent::ToolResultsRecovered {
                index,
                results: results.clone(),
            })?;
            messages.splice(index..index, results);
            self.context = None;
        }
        Ok(())
    }
}

/// Repair one incomplete batch without guessing whether its tools executed.
pub(crate) fn missing_tool_results(messages: &[Message]) -> Option<(usize, Vec<Message>)> {
    for (index, message) in messages.iter().enumerate() {
        if message.role != Role::Assistant || message.tool_calls.is_empty() {
            continue;
        }
        let end = index
            + 1
            + messages[index + 1..]
                .iter()
                .take_while(|m| m.role == Role::Tool)
                .count();
        let mut results = Vec::new();
        for call in &message.tool_calls {
            if messages[index + 1..end]
                .iter()
                .any(|result| result.tool_call_id.as_deref() == Some(call.id.as_str()))
            {
                continue;
            }
            results.push(Message::tool_result(
                &call.id, &call.name,
                "[tool result unavailable after an interrupted session or storage failure; the tool may have executed. Check its effects before retrying.]".into(),
                true,
            ));
        }
        if !results.is_empty() {
            return Some((end, results));
        }
    }
    None
}
