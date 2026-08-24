mod manager;
mod names;
pub(crate) mod presets;
mod types;

pub(crate) use manager::SubagentManager;
pub(crate) use presets::AgentPreset;
#[cfg(test)]
pub(crate) use types::{QueuedSubagentMessage, SubagentId};
pub(crate) use types::{
    RunOrigin, RunOutcome, SubagentSnapshot, SubagentStatus, SubagentTranscriptItem,
    sanitize_preview,
};
