//! Inspection commands that answer questions without running a turn:
//! readiness, models, sessions, presets, and tools. Each prints text for
//! people or one JSON object for scripts and other agents.

mod check;
mod listing;
mod models;

pub(crate) use check::run as check;
pub(crate) use listing::{agents as list_agents, sessions as list_sessions, tools as list_tools};
pub(crate) use models::run as list_models;
