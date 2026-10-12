//! Tool registry: builtins plus custom tool manifests discovered from disk.
//!
//! The registry is rescanned every agent-loop iteration, so a tool the model
//! just defined is usable on its next step. Custom tools never replace
//! builtins; between custom tools, `./.yawl/tools` overrides `~/.yawl/tools`.

mod catalog;
mod custom;
pub mod exec;
mod files;
mod git;
mod mode;
mod orchestration;
mod output;
mod planning_shell;
mod registry;
mod shell;
mod skills;
mod user_input;
mod web;

use serde_json::Value;

pub use catalog::CatalogCache;
pub(crate) use custom::DEFINE_TOOL_NAME;
pub use custom::has_project_tools;
pub(crate) use output::save_artifact;
pub use registry::{Registry, ToolOutcome};
use registry::{ToolEntry, ToolImpl};
pub(crate) use user_input::{QuestionBroker, QuestionSnapshot};
/// Short plain-language purposes for builtin tools named in restriction
/// notices.
fn capability(name: &str) -> Option<&'static str> {
    Some(match name {
        "read_file" => "read files",
        "list_files" => "list files",
        "search_files" => "search file contents",
        "git_inspect" => "read-only Git status, diffs, and history",
        "read_skill" => "load skill instructions",
        "shell" => "run shell commands",
        "write_file" => "create or overwrite files",
        "edit_file" => "edit files",
        "web_search" => "search the web",
        "web_fetch" => "fetch web pages",
        _ => return None,
    })
}

#[cfg(test)]
pub(crate) use user_input::{QuestionOption, UserQuestion};

/// Builtin names a custom tool may never take, whether or not that builtin
/// is advertised in the current scan.
const RESERVED_TOOL_NAMES: &[&str] = &[
    "shell",
    "read_file",
    "write_file",
    "edit_file",
    custom::DEFINE_TOOL_NAME,
    "list_files",
    "search_files",
    "git_inspect",
    "read_skill",
    "shell_list",
    "shell_output",
    "shell_stop",
    "subagent_spawn",
    "subagent_send",
    "subagent_wait",
    "subagent_cancel",
    "subagent_list",
    "goal_complete",
    "request_user_input",
    "plan_complete",
    "plan_action",
    "plan_implemented",
];
const WEB_TOOL_NAMES: &[&str] = &["web_search", "web_fetch"];
pub(crate) const GOAL_COMPLETE_TOOL_NAME: &str = "goal_complete";
pub(crate) const USER_INPUT_TOOL_NAME: &str = user_input::TOOL_NAME;
pub(crate) const PLAN_COMPLETE_TOOL_NAME: &str = "plan_complete";
pub(crate) const PLAN_ACTION_TOOL_NAME: &str = "plan_action";
pub(crate) const PLAN_IMPLEMENTED_TOOL_NAME: &str = "plan_implemented";

/// Tells the model which calls run while planning, since the advertised
/// tool list stays unrestricted.
pub(crate) fn planning_restriction_note() -> String {
    planning_shell::restriction_note()
}

pub(crate) fn validate_user_input(arguments: &str) -> Result<usize, String> {
    let value: Value = serde_json::from_str(arguments)
        .map_err(|error| format!("invalid tool arguments json: {error}"))?;
    user_input::parse_questions(&value).map(|questions| questions.len())
}

pub(crate) fn is_private_tool_name(name: &str) -> bool {
    matches!(
        name,
        GOAL_COMPLETE_TOOL_NAME
            | PLAN_COMPLETE_TOOL_NAME
            | PLAN_ACTION_TOOL_NAME
            | PLAN_IMPLEMENTED_TOOL_NAME
    )
}

fn reserved_tool_name(web_browsing: bool, name: &str) -> bool {
    RESERVED_TOOL_NAMES.contains(&name) || (web_browsing && WEB_TOOL_NAMES.contains(&name))
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolOutcome> {
    args[key]
        .as_str()
        .ok_or_else(|| ToolOutcome::error(format!("missing required string argument '{key}'")))
}
