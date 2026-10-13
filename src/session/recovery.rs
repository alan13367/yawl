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
        self.state.pending_undo.as_deref()
    }

    pub(crate) fn begin_undo(&mut self, undo: PendingUndo) -> Result<(), Error> {
        if self.state.pending_undo.is_some() {
            return Err(Error::Protocol("another undo is unfinished".into()));
        }
        self.record(SessionEvent::UndoStarted { undo })
    }

    pub(crate) fn finish_undo(&mut self) -> Result<(), Error> {
        self.record(SessionEvent::UndoFinished)
    }

    pub(crate) fn context(&self) -> Option<&ContextUsage> {
        self.state.context.as_ref()
    }

    pub(crate) fn record_context(&mut self, context: ContextUsage) -> Result<(), Error> {
        self.record(SessionEvent::Context { context })
    }

    /// Adds an error result for every tool call left without one, so the
    /// history is valid for the next request. Returns whether it changed.
    pub(crate) fn repair_tool_results(&mut self) -> Result<bool, Error> {
        let mut repaired = false;
        while let Some((index, results)) = missing_tool_results(self.messages()) {
            self.record(SessionEvent::ToolResultsRecovered { index, results })?;
            repaired = true;
        }
        Ok(repaired)
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
