mod manager;
mod types;

pub(crate) use manager::SubagentManager;
#[cfg(test)]
pub(crate) use types::{QueuedSubagentMessage, SubagentId};
pub(crate) use types::{
    RunOrigin, RunOutcome, SubagentSnapshot, SubagentStatus, SubagentTranscriptItem,
    sanitize_preview,
};
