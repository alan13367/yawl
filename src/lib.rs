//! Yawl — a minimal, self-extending AI agent harness.
//!
//! One Cargo package, blocking I/O, and a small direct dependency set. The
//! model extends Yawl at runtime by writing executable tools; the harness
//! never needs recompiling to gain capabilities.

mod agent;
mod app;
mod background;
mod cancellation;
mod checkpoint;
mod cli;
mod compaction;
mod config;
mod doctor;
mod error;
mod exit_status;
mod image;
mod inspect;
mod model;
mod onboarding;
mod paths;
mod print_mode;
mod project_trust;
mod prompt;
mod provider;
mod session;
mod skill_export;
mod skills;
mod subagent;
mod terminal_mode;
#[cfg(test)]
mod test_support;
mod text;
mod tools;
mod trust;
mod tui;

pub use app::main;

// Process-wide Ctrl+C state and handler installation live in `cancellation`;
// these paths stay stable for the front ends and tests.
pub(crate) use cancellation::{
    install_interrupt_handler, process_interrupted as interrupted,
    set_process_interrupted as set_interrupted,
};
