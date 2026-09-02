//! Tool registry: builtins plus exec tools discovered from disk.
//!
//! The registry is rescanned every agent-loop iteration, so a tool the model
//! just wrote is usable on its next turn. On name collisions the last scan
//! wins (builtins < `~/.yawl/tools` < `./.yawl/tools`).

pub mod exec;
mod planning_shell;
mod user_input;
mod web;

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::background::{BackgroundProcessManager, OutputRead, StartSpec};
use crate::config::Config;
use crate::provider::ToolSpec;
use crate::skills::Skill;
use crate::subagent::presets::discover as discover_presets;
use crate::subagent::{AgentPreset, RunOrigin, SubagentManager};

pub use exec::DescribeCache;
pub(crate) use user_input::{QuestionBroker, QuestionSnapshot};
#[cfg(test)]
pub(crate) use user_input::{QuestionOption, UserQuestion};

/// Cap on tool result size fed back to the model.
const MAX_RESULT_CHARS: usize = 60_000;
/// Cap on `read_file` input size. Anything larger truncates to
/// `MAX_RESULT_CHARS` anyway, so reading it in full only wastes memory.
const MAX_READ_FILE_BYTES: u64 = 1024 * 1024;
const SHELL_DEFAULT_TIMEOUT_SECS: u64 = 120;

enum ToolImpl {
    Shell,
    ShellList,
    ShellOutput,
    ShellStop,
    ReadFile,
    ReadSkill,
    WriteFile,
    EditFile,
    WebSearch,
    WebFetch,
    GoalComplete,
    PlanningShell,
    UserInput(QuestionBroker),
    PlanComplete,
    PlanAction,
    PlanImplemented,
    Exec(exec::ExecTool),
    Subagent(SubagentTool),
}

#[derive(Clone, Copy)]
enum SubagentTool {
    Spawn,
    Send,
    Wait,
    Cancel,
    List,
}

struct SubagentContext {
    manager: SubagentManager,
    config: Config,
    parent_model: String,
    presets: Vec<AgentPreset>,
}

struct ToolEntry {
    spec: ToolSpec,
    imp: ToolImpl,
}

pub struct ToolOutcome {
    pub content: String,
    pub images: Vec<crate::provider::ImageContent>,
    pub is_error: bool,
}

impl ToolOutcome {
    fn error(msg: impl Into<String>) -> ToolOutcome {
        ToolOutcome {
            content: msg.into(),
            images: Vec::new(),
            is_error: true,
        }
    }

    fn ok(content: String) -> ToolOutcome {
        ToolOutcome {
            content,
            images: Vec::new(),
            is_error: false,
        }
    }

    fn image(content: String, image: crate::provider::ImageContent) -> ToolOutcome {
        ToolOutcome {
            content,
            images: vec![image],
            is_error: false,
        }
    }
}

pub struct Registry {
    entries: Vec<ToolEntry>,
    pub warnings: Vec<String>,
    skills: Vec<Skill>,
    subagents: Option<SubagentContext>,
    background: Option<BackgroundProcessManager>,
    web: Option<web::WebTools>,
}

impl Registry {
    /// Scans builtins + exec tool directories. Called every loop iteration;
    /// `cache` avoids respawning `--describe` for unchanged tools.
    pub fn scan(config: &Config, cache: &mut DescribeCache) -> Registry {
        Self::scan_inner(config, cache, None)
    }

    /// Scans the tools advertised by the persistent main agent without
    /// creating a live process manager. Used by `yawl --list-tools`.
    pub fn scan_for_main_listing(config: &Config, cache: &mut DescribeCache) -> Registry {
        let mut registry = Self::scan(config, cache);
        if let Some(shell) = registry
            .entries
            .iter_mut()
            .find(|entry| entry.spec.name == "shell" && matches!(&entry.imp, ToolImpl::Shell))
        {
            *shell = shell_entry(true);
        }
        registry.entries.extend(background_entries());
        registry
    }

    fn scan_inner(
        config: &Config,
        cache: &mut DescribeCache,
        background: Option<BackgroundProcessManager>,
    ) -> Registry {
        let mut registry = Registry {
            entries: builtins(background.is_some()),
            warnings: Vec::new(),
            skills: Vec::new(),
            subagents: None,
            background,
            web: config.web_browsing.then(|| web::WebTools::new(config)),
        };
        if config.web_browsing {
            registry
                .entries
                .extend(web_entries(config.web_search_provider));
        }
        if registry.background.is_some() {
            registry.entries.extend(background_entries());
        }
        let catalog = crate::skills::discover(config);
        registry.warnings.extend(catalog.warnings);
        registry.skills = catalog
            .skills
            .into_iter()
            .filter(|skill| !skill.disable_model_invocation)
            .collect();
        if !registry.skills.is_empty() {
            registry.entries.push(read_skill_entry());
        }
        for dir in config.tool_dirs() {
            let (tools, warnings) = exec::scan_dir(&dir, cache);
            registry.warnings.extend(warnings);
            for tool in tools {
                if reserved_tool_name(config, &tool.spec.name) {
                    registry.warnings.push(format!(
                        "{}: tool name '{}' is reserved by Yawl",
                        tool.path.display(),
                        tool.spec.name
                    ));
                    continue;
                }
                registry.insert(ToolEntry {
                    spec: tool.spec.clone(),
                    imp: ToolImpl::Exec(tool),
                });
            }
        }
        registry
    }

    #[cfg(test)]
    pub(crate) fn scan_with_subagents(
        config: &Config,
        cache: &mut DescribeCache,
        manager: SubagentManager,
        parent_model: &str,
    ) -> Registry {
        let mut registry = Self::scan(config, cache);
        registry.enable_subagents(config, manager, parent_model);
        registry
    }

    fn enable_subagents(&mut self, config: &Config, manager: SubagentManager, parent_model: &str) {
        let (presets, warnings) = discover_presets(config);
        self.warnings.extend(warnings);
        self.entries.extend(subagent_tools(&presets));
        self.subagents = Some(SubagentContext {
            manager,
            config: config.clone(),
            parent_model: parent_model.to_string(),
            presets,
        });
    }

    pub(crate) fn scan_with_background(
        config: &Config,
        cache: &mut DescribeCache,
        background: BackgroundProcessManager,
    ) -> Registry {
        Self::scan_inner(config, cache, Some(background))
    }

    pub(crate) fn scan_with_subagents_and_background(
        config: &Config,
        cache: &mut DescribeCache,
        manager: SubagentManager,
        parent_model: &str,
        background: BackgroundProcessManager,
    ) -> Registry {
        let mut registry = Self::scan_inner(config, cache, Some(background));
        registry.enable_subagents(config, manager, parent_model);
        registry
    }

    /// Drops every entry whose name is not listed. Used for preset
    /// subagents, whose tool allowlist is enforced at scan time.
    pub(crate) fn retain_names(&mut self, names: &[String]) {
        self.entries
            .retain(|entry| names.contains(&entry.spec.name));
        if !names.iter().any(|name| name == "read_skill") {
            self.skills.clear();
        }
    }

    fn insert(&mut self, entry: ToolEntry) {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|e| e.spec.name == entry.spec.name)
        {
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.entries.iter().map(|e| e.spec.clone()).collect()
    }

    pub(crate) fn advertise_goal_complete(&mut self) {
        self.insert(goal_complete_entry());
    }

    pub(crate) fn advertise_user_input(&mut self, broker: QuestionBroker) {
        self.insert(user_input_entry(broker));
    }

    pub(crate) fn advertise_plan_complete(&mut self) {
        self.insert(plan_complete_entry());
    }

    pub(crate) fn advertise_plan_action(&mut self) {
        self.insert(plan_action_entry());
    }

    pub(crate) fn advertise_plan_implemented(&mut self) {
        self.insert(plan_implemented_entry());
    }

    /// Keeps only read tools and replaces the unrestricted shell with its
    /// planning-only inspection gate.
    pub(crate) fn retain_for_planning(&mut self) {
        self.entries.retain(|entry| {
            matches!(
                &entry.imp,
                ToolImpl::Shell
                    | ToolImpl::ReadFile
                    | ToolImpl::ReadSkill
                    | ToolImpl::WebSearch
                    | ToolImpl::WebFetch
                    | ToolImpl::UserInput(_)
                    | ToolImpl::PlanComplete
            )
        });
        self.insert(planning_shell_entry());
    }

    pub(crate) fn skills(&self) -> &[Skill] {
        &self.skills
    }

    pub(crate) fn has_web_tools(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(&entry.imp, ToolImpl::WebSearch | ToolImpl::WebFetch))
    }

    /// Name, description, and origin for `/tools` and `--list-tools`.
    pub fn describe_all(&self) -> Vec<(String, String, String)> {
        self.entries
            .iter()
            .filter(|entry| !is_private_tool(&entry.imp))
            .map(|e| {
                let origin = match &e.imp {
                    ToolImpl::Exec(t) => t.path.display().to_string(),
                    ToolImpl::Subagent(_) => "orchestration".to_string(),
                    ToolImpl::ReadSkill => "skills".to_string(),
                    ToolImpl::GoalComplete => "internal".to_string(),
                    ToolImpl::PlanComplete | ToolImpl::PlanAction | ToolImpl::PlanImplemented => {
                        "internal".to_string()
                    }
                    ToolImpl::UserInput(_) => "interactive".to_string(),
                    ToolImpl::ShellList | ToolImpl::ShellOutput | ToolImpl::ShellStop => {
                        "builtin".to_string()
                    }
                    _ => "builtin".to_string(),
                };
                (e.spec.name.clone(), e.spec.description.clone(), origin)
            })
            .collect()
    }

    pub fn execute(&self, name: &str, args_json: &str, session_id: &str) -> ToolOutcome {
        self.execute_with_capabilities(name, args_json, session_id, false)
    }

    pub(crate) fn execute_with_capabilities(
        &self,
        name: &str,
        args_json: &str,
        session_id: &str,
        supports_images: bool,
    ) -> ToolOutcome {
        let Some(entry) = self.entries.iter().find(|e| e.spec.name == name) else {
            return ToolOutcome::error(format!(
                "unknown tool '{name}'; available: {}",
                self.entries
                    .iter()
                    .map(|e| e.spec.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        };
        let args: Value = match serde_json::from_str(args_json) {
            Ok(v) => v,
            Err(e) => return ToolOutcome::error(format!("invalid tool arguments json: {e}")),
        };
        let mut outcome = match &entry.imp {
            ToolImpl::Shell => shell(&args, self.background.as_ref()),
            ToolImpl::ShellList => shell_list(self.background.as_ref()),
            ToolImpl::ShellOutput => shell_output(self.background.as_ref(), &args),
            ToolImpl::ShellStop => shell_stop(self.background.as_ref(), &args),
            ToolImpl::ReadFile => read_file_for_model(&args, supports_images),
            ToolImpl::ReadSkill => read_skill(&self.skills, &args),
            ToolImpl::WriteFile => write_file(&args),
            ToolImpl::EditFile => edit_file(&args),
            ToolImpl::WebSearch => self.execute_web(&args, true),
            ToolImpl::WebFetch => self.execute_web(&args, false),
            ToolImpl::GoalComplete => goal_complete_outcome(&args),
            ToolImpl::PlanningShell => match planning_shell::prepare(&args) {
                Ok(args) => {
                    shell_with_path(&args, None, planning_shell::inspection_path().as_deref())
                }
                Err(error) => ToolOutcome::error(error),
            },
            ToolImpl::UserInput(broker) => match user_input::parse_questions(&args)
                .and_then(|questions| broker.ask(questions))
            {
                Ok(content) => ToolOutcome::ok(content),
                Err(error) => ToolOutcome::error(error),
            },
            ToolImpl::PlanComplete => plan_complete_outcome(&args),
            ToolImpl::PlanAction => plan_action_outcome(&args),
            ToolImpl::PlanImplemented => plan_implemented_outcome(&args),
            ToolImpl::Exec(tool) => {
                let (content, is_error) = exec::invoke(tool, args_json, session_id);
                ToolOutcome {
                    content,
                    images: Vec::new(),
                    is_error,
                }
            }
            ToolImpl::Subagent(tool) => self.execute_subagent(*tool, &args),
        };
        truncate_result(&mut outcome.content);
        if outcome.content.is_empty() {
            outcome.content = if outcome.is_error {
                "(no output)".to_string()
            } else {
                "(no output; command succeeded)".to_string()
            };
        }
        outcome
    }

    fn execute_web(&self, args: &Value, search: bool) -> ToolOutcome {
        let Some(web) = &self.web else {
            return ToolOutcome::error("web browsing is disabled");
        };
        let result = if search {
            str_arg(args, "query").and_then(|query| web.search(query).map_err(ToolOutcome::error))
        } else {
            str_arg(args, "url").and_then(|url| web.fetch(url).map_err(ToolOutcome::error))
        };
        match result {
            Ok(content) => ToolOutcome::ok(content),
            Err(error) => error,
        }
    }

    fn execute_subagent(&self, tool: SubagentTool, args: &Value) -> ToolOutcome {
        let Some(context) = &self.subagents else {
            return ToolOutcome::error("subagent orchestration is not available");
        };
        let result = match tool {
            SubagentTool::Spawn => {
                if args.get("model").is_some() {
                    return ToolOutcome::error(
                        "'model' is not accepted; subagents use the configured model or inherit the active parent model",
                    );
                }
                let prompt = str_arg(args, "prompt").map_err(|error| error.content);
                let required_tools = string_array(args, "required_tools");
                let name = match args.get("name") {
                    Some(value) => value
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| "'name' must be a string when provided".to_string()),
                    None => Ok(String::new()),
                };
                prompt.and_then(|prompt| {
                    required_tools.and_then(|required_tools| {
                        name.and_then(|name| {
                            let preset: Option<&AgentPreset> = match args.get("agent") {
                                Some(value) => {
                                    let agent = value.as_str().ok_or_else(|| {
                                        "'agent' must be a string when provided".to_string()
                                    })?;
                                    Some(
                                        context
                                            .presets
                                            .iter()
                                            .find(|preset| preset.name == agent)
                                            .ok_or_else(|| {
                                                format!(
                                                    "unknown agent '{agent}'; available: {}",
                                                    context
                                                        .presets
                                                        .iter()
                                                        .map(|preset| preset.name.as_str())
                                                        .collect::<Vec<_>>()
                                                        .join(", ")
                                                )
                                            })?,
                                    )
                                }
                                None => None,
                            };
                            if let Some(preset) = preset {
                                validate_preset_capabilities(preset, &required_tools)?;
                            }
                            let supplied_name = (!name.trim().is_empty()).then_some(name.as_str());
                            context
                                .manager
                                .spawn(
                                    context.config.clone(),
                                    &context.parent_model,
                                    supplied_name,
                                    prompt,
                                    preset,
                                )
                                .map(|id| format!("started {id}"))
                        })
                    })
                })
            }
            SubagentTool::Send => {
                let id = str_arg(args, "id").map_err(|error| error.content);
                let message = str_arg(args, "message").map_err(|error| error.content);
                id.and_then(|id| {
                    message.and_then(|message| context.manager.send(id, message, RunOrigin::Model))
                })
            }
            SubagentTool::Wait => string_array(args, "ids").and_then(|ids| {
                let timeout = match args.get("timeout_secs") {
                    Some(value) => Some(
                        value
                            .as_u64()
                            .ok_or_else(|| "'timeout_secs' must be an integer".to_string())?,
                    ),
                    None => None,
                };
                context.manager.wait(&ids, timeout)
            }),
            SubagentTool::Cancel => {
                string_array(args, "ids").and_then(|ids| context.manager.cancel(&ids, false))
            }
            SubagentTool::List => match args.get("id") {
                Some(value) => value
                    .as_str()
                    .ok_or_else(|| "'id' must be a string".to_string())
                    .and_then(|id| context.manager.list(Some(id))),
                None => context.manager.list(None),
            },
        };
        match result {
            Ok(content) => ToolOutcome::ok(content),
            Err(error) => ToolOutcome::error(error),
        }
    }
}

const RESERVED_TOOL_NAMES: &[&str] = &[
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

pub(crate) fn validate_user_input(arguments: &str) -> Result<usize, String> {
    let value: Value = serde_json::from_str(arguments)
        .map_err(|error| format!("invalid tool arguments json: {error}"))?;
    user_input::parse_questions(&value).map(|questions| questions.len())
}

fn is_private_tool(tool: &ToolImpl) -> bool {
    matches!(
        tool,
        ToolImpl::GoalComplete
            | ToolImpl::PlanComplete
            | ToolImpl::PlanAction
            | ToolImpl::PlanImplemented
    )
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

fn reserved_tool_name(config: &Config, name: &str) -> bool {
    RESERVED_TOOL_NAMES.contains(&name) || (config.web_browsing && WEB_TOOL_NAMES.contains(&name))
}

fn read_skill_entry() -> ToolEntry {
    ToolEntry {
        spec: ToolSpec {
            name: "read_skill".into(),
            description: "Load full instructions for an advertised skill.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Exact advertised skill name"}
                },
                "required": ["name"]
            }),
        },
        imp: ToolImpl::ReadSkill,
    }
}

fn goal_complete_entry() -> ToolEntry {
    ToolEntry {
        spec: ToolSpec {
            name: GOAL_COMPLETE_TOOL_NAME.into(),
            description: "Finish the active goal with the final user-facing answer. This must be the only tool call in that step.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "result": {
                        "type": "string",
                        "description": "Final user-facing answer for the completed goal"
                    }
                },
                "required": ["result"]
            }),
        },
        imp: ToolImpl::GoalComplete,
    }
}

fn planning_shell_entry() -> ToolEntry {
    ToolEntry {
        spec: ToolSpec {
            name: "shell".into(),
            description: "Run a read-only repository inspection command. Pipelines are allowed between: basename, cat, cut, dirname, du, git, grep, head, ls, pwd, readlink, realpath, rg, sed, stat, tail, tr, and wc. Git is limited to read-only subcommands, sed to print-only ranges, and rg cannot use preprocessors. Redirection, chaining, expansion, background execution, and other commands are rejected."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Seconds; defaults to 120."
                    }
                },
                "required": ["command"]
            }),
        },
        imp: ToolImpl::PlanningShell,
    }
}

fn goal_complete_outcome(args: &Value) -> ToolOutcome {
    match args.get("result").and_then(Value::as_str).map(str::trim) {
        Some(result) if !result.is_empty() => ToolOutcome::ok(result.to_string()),
        _ => ToolOutcome::error("goal_complete requires a non-empty string 'result'"),
    }
}

fn user_input_entry(broker: QuestionBroker) -> ToolEntry {
    ToolEntry {
        spec: ToolSpec {
            name: USER_INPUT_TOOL_NAME.into(),
            description: "Ask the user one to three multiple-choice questions. Each question needs 2 or 3 options and one recommended option. Yawl adds an open-answer choice automatically; custom replies have a null option_index and their text in answer. Set the recommendation with the recommended index; do not add '(Recommended)' to an option label. This must be the only tool call in its step. If the result says timed_out, do not ask again in this turn.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 3,
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string"},
                                "question": {"type": "string"},
                                "options": {
                                    "type": "array",
                                    "minItems": 2,
                                    "maxItems": 3,
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {
                                                "type": "string",
                                                "description": "Short answer label without a recommendation marker"
                                            },
                                            "description": {"type": "string"}
                                        },
                                        "required": ["label", "description"]
                                    }
                                },
                                "recommended": {"type": "integer", "minimum": 0, "maximum": 2}
                            },
                            "required": ["id", "question", "options", "recommended"]
                        }
                    }
                },
                "required": ["questions"]
            }),
        },
        imp: ToolImpl::UserInput(broker),
    }
}

fn plan_complete_entry() -> ToolEntry {
    ToolEntry {
        spec: ToolSpec {
            name: PLAN_COMPLETE_TOOL_NAME.into(),
            description: "Finish planning with the complete Markdown plan. This must be the only tool call in its step.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"plan": {"type": "string"}},
                "required": ["plan"]
            }),
        },
        imp: ToolImpl::PlanComplete,
    }
}

fn plan_action_entry() -> ToolEntry {
    ToolEntry {
        spec: ToolSpec {
            name: PLAN_ACTION_TOOL_NAME.into(),
            description: "Classify the user's latest request against the active plan. Use unrelated to continue the request as a normal turn with the full tool set. This must be the only tool call in its step.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"action": {"type": "string", "enum": ["revise", "implement", "unrelated"]}},
                "required": ["action"]
            }),
        },
        imp: ToolImpl::PlanAction,
    }
}

fn plan_implemented_entry() -> ToolEntry {
    ToolEntry {
        spec: ToolSpec {
            name: PLAN_IMPLEMENTED_TOOL_NAME.into(),
            description: "Finish implementation of the active plan with the final user-facing result. This must be the only tool call in its step.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"result": {"type": "string"}},
                "required": ["result"]
            }),
        },
        imp: ToolImpl::PlanImplemented,
    }
}

fn plan_complete_outcome(args: &Value) -> ToolOutcome {
    non_empty_arg(args, "plan", PLAN_COMPLETE_TOOL_NAME)
}

fn plan_action_outcome(args: &Value) -> ToolOutcome {
    match args.get("action").and_then(Value::as_str) {
        Some(action @ ("revise" | "implement" | "unrelated")) => ToolOutcome::ok(action.into()),
        _ => {
            ToolOutcome::error("plan_action requires action 'revise', 'implement', or 'unrelated'")
        }
    }
}

fn plan_implemented_outcome(args: &Value) -> ToolOutcome {
    non_empty_arg(args, "result", PLAN_IMPLEMENTED_TOOL_NAME)
}

fn non_empty_arg(args: &Value, key: &str, tool: &str) -> ToolOutcome {
    match args.get(key).and_then(Value::as_str).map(str::trim) {
        Some(value) if !value.is_empty() => ToolOutcome::ok(value.to_string()),
        _ => ToolOutcome::error(format!("{tool} requires a non-empty string '{key}'")),
    }
}

fn subagent_tools(presets: &[AgentPreset]) -> Vec<ToolEntry> {
    let tool = |name: &str, description: &str, input_schema: Value, imp| ToolEntry {
        spec: ToolSpec {
            name: name.into(),
            description: description.into(),
            input_schema,
        },
        imp: ToolImpl::Subagent(imp),
    };
    let available_agents = presets
        .iter()
        .map(|preset| {
            let tools = preset
                .tools
                .as_ref()
                .map_or_else(|| "all tools".to_string(), |tools| tools.join("+"));
            format!("{} ({}): {}", preset.name, tools, preset.description)
        })
        .collect::<Vec<_>>()
        .join("; ");
    let agent_names = presets
        .iter()
        .map(|preset| preset.name.clone())
        .collect::<Vec<_>>();
    let spawn_description = format!(
        "Start a background subagent and return its ID. Prompt contract: # Target (paths, \
         ownership, non-goals), # Change, # Acceptance. Declare all required tools; omit agent for \
         writes, commands, or unsupported tools. scout is read-only. Never set a model. Agents: \
         {available_agents}."
    );
    vec![
        tool(
            "subagent_spawn",
            &spawn_description,
            json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string"},
                    "required_tools": {
                        "type": "array",
                        "items": {"type": "string"},
                        "maxItems": 64,
                        "description": "All tools the child needs; [] only for a tool-free answer."
                    },
                    "name": {"type": "string"},
                    "agent": {
                        "type": "string",
                        "enum": agent_names,
                        "description": "Optional preset. Omit for the default agent. Scout is read-only."
                    }
                },
                "required": ["prompt", "required_tools"]
            }),
            SubagentTool::Spawn,
        ),
        tool(
            "subagent_send",
            "Send another turn to a subagent; restart it if settled.",
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "message": {"type": "string"}
                },
                "required": ["id", "message"]
            }),
            SubagentTool::Send,
        ),
        tool(
            "subagent_wait",
            "Wait for every ID to finish or fail. Omit timeout_secs to block; set it only for a \
             bounded status check. Settled runs include their full result.",
            json!({
                "type": "object",
                "properties": {
                    "ids": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 64},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 300, "description": "Optional bounded wait; omit to block until every ID settles."}
                },
                "required": ["ids"]
            }),
            SubagentTool::Wait,
        ),
        tool(
            "subagent_cancel",
            "Cancel runs and queued work for the IDs, retaining partial transcripts. Use only when \
             the work is no longer needed.",
            json!({
                "type": "object",
                "properties": {
                    "ids": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 64}
                },
                "required": ["ids"]
            }),
            SubagentTool::Cancel,
        ),
        tool(
            "subagent_list",
            "List all subagents, or full status and result for one ID.",
            json!({
                "type": "object",
                "properties": {"id": {"type": "string"}}
            }),
            SubagentTool::List,
        ),
    ]
}

fn validate_preset_capabilities(
    preset: &AgentPreset,
    required_tools: &[String],
) -> Result<(), String> {
    let Some(allowed_tools) = &preset.tools else {
        return Ok(());
    };
    let missing = required_tools
        .iter()
        .filter(|required| !allowed_tools.contains(required))
        .map(String::as_str)
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "agent '{}' does not provide required tool(s): {}; omit 'agent' to use the default agent or choose a compatible preset",
            preset.name,
            missing.join(", ")
        ))
    }
}

fn string_array(args: &Value, key: &str) -> Result<Vec<String>, String> {
    let values = args
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("missing required array argument '{key}'"))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("'{key}' must contain only strings"))
        })
        .collect()
}

fn truncate_result(content: &mut String) {
    if let Some((cut, _)) = content.char_indices().nth(MAX_RESULT_CHARS) {
        content.truncate(cut);
        content.push_str("\n[output truncated]");
    }
}

fn builtins(background: bool) -> Vec<ToolEntry> {
    vec![
        shell_entry(background),
        ToolEntry {
            spec: ToolSpec {
                name: "read_file".into(),
                description: "Read a UTF-8 text file or a PNG, JPEG, GIF, or WebP image.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path (absolute or relative to cwd)"}
                    },
                    "required": ["path"]
                }),
            },
            imp: ToolImpl::ReadFile,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "write_file".into(),
                description:
                    "Write a file, creating parent directories; replaces existing content.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }),
            },
            imp: ToolImpl::WriteFile,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "edit_file".into(),
                description: "Replace one exact `old_string` occurrence; include enough context for uniqueness."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "old_string": {"type": "string"},
                        "new_string": {"type": "string"}
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
            },
            imp: ToolImpl::EditFile,
        },
    ]
}

fn web_entries(provider: crate::config::WebSearchProvider) -> Vec<ToolEntry> {
    web::WebTools::specs(provider)
        .into_iter()
        .map(|spec| {
            let imp = match spec.name.as_str() {
                "web_search" => ToolImpl::WebSearch,
                "web_fetch" => ToolImpl::WebFetch,
                _ => unreachable!("web module returned an unknown builtin"),
            };
            ToolEntry { spec, imp }
        })
        .collect()
}

fn shell_entry(background: bool) -> ToolEntry {
    let mut properties = json!({
        "command": {"type": "string"},
        "timeout_secs": {"type": "integer", "minimum": 1, "description": "Seconds; foreground defaults to 120, background to unlimited."}
    });
    let description = if background {
        properties["background"] = json!({
            "type": "boolean",
            "description": "Run in background and return a bg-N ID"
        });
        properties["name"] = json!({
            "type": "string",
            "maxLength": 80,
            "description": "Optional /ps label"
        });
        "Run `sh -c` in the working directory. Use background=true for long commands, then shell_output, shell_list, or shell_stop."
    } else {
        "Run foreground `sh -c` in the working directory; return stdout or the failure."
    };
    ToolEntry {
        spec: ToolSpec {
            name: "shell".into(),
            description: description.into(),
            input_schema: json!({
                "type": "object",
                "properties": properties,
                "required": ["command"]
            }),
        },
        imp: ToolImpl::Shell,
    }
}

fn background_entries() -> Vec<ToolEntry> {
    vec![
        ToolEntry {
            spec: ToolSpec {
                name: "shell_list".into(),
                description: "List background commands with ID, status, PID, elapsed time, label, and command.".into(),
                input_schema: json!({"type": "object", "properties": {}}),
            },
            imp: ToolImpl::ShellList,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "shell_output".into(),
                description: "Read new background-command output. Reuse next_cursor; wait_secs may wait for output or completion.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "bg-N ID"},
                        "cursor": {"type": "integer", "minimum": 0, "description": "Previous cursor; default 0"},
                        "wait_secs": {"type": "integer", "minimum": 0, "maximum": 30, "description": "Wait seconds; default 0"}
                    },
                    "required": ["id"]
                }),
            },
            imp: ToolImpl::ShellOutput,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "shell_stop".into(),
                description: "Gracefully stop a background process group; settled commands return status.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"id": {"type": "string", "description": "bg-N ID"}},
                    "required": ["id"]
                }),
            },
            imp: ToolImpl::ShellStop,
        },
    ]
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolOutcome> {
    args[key]
        .as_str()
        .ok_or_else(|| ToolOutcome::error(format!("missing required string argument '{key}'")))
}

fn shell(args: &Value, background: Option<&BackgroundProcessManager>) -> ToolOutcome {
    shell_with_path(args, background, None)
}

fn shell_with_path(
    args: &Value,
    background: Option<&BackgroundProcessManager>,
    path: Option<&std::ffi::OsStr>,
) -> ToolOutcome {
    let command = match str_arg(args, "command") {
        Ok(c) => c,
        Err(e) => return e,
    };
    let run_in_background = match args.get("background") {
        Some(Value::Bool(value)) => *value,
        None => false,
        Some(_) => return ToolOutcome::error("'background' must be a boolean when provided"),
    };
    if run_in_background {
        let Some(background) = background else {
            return ToolOutcome::error("background shell execution is not available");
        };
        let name = match args.get("name") {
            Some(Value::String(name)) if name.trim().chars().count() > 80 => {
                return ToolOutcome::error("'name' must be at most 80 characters");
            }
            Some(Value::String(name)) if !name.trim().is_empty() => Some(name.trim().to_string()),
            Some(Value::String(_)) | None => None,
            Some(_) => return ToolOutcome::error("'name' must be a string when provided"),
        };
        let timeout = match args.get("timeout_secs") {
            Some(value) => match value.as_u64() {
                Some(0) | None => {
                    return ToolOutcome::error("'timeout_secs' must be a positive integer");
                }
                Some(seconds) => Some(Duration::from_secs(seconds)),
            },
            None => None,
        };
        return match background.start(StartSpec {
            command: command.to_string(),
            name: name.clone(),
            cwd: crate::config::working_dir(),
            timeout,
        }) {
            Ok(started) => ToolOutcome::ok(format!(
                "started {} (pid {}){} in the background\nnext_cursor: 0",
                started.id,
                started.pid,
                name.map_or_else(String::new, |name| format!(" as {name}"))
            )),
            Err(error) => ToolOutcome::error(error),
        };
    }
    let timeout_secs = match args.get("timeout_secs") {
        Some(value) => match value.as_u64() {
            Some(0) | None => {
                return ToolOutcome::error("'timeout_secs' must be a positive integer");
            }
            Some(seconds) => seconds,
        },
        None => SHELL_DEFAULT_TIMEOUT_SECS,
    };
    let timeout = Duration::from_secs(timeout_secs);
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    if let Some(path) = path {
        cmd.env("PATH", path);
    }
    match exec::run_with_timeout(cmd, None, timeout) {
        Ok(result) => {
            let (content, is_error) = exec::render_result(&result, timeout);
            ToolOutcome {
                content,
                images: Vec::new(),
                is_error,
            }
        }
        Err(e) => ToolOutcome::error(format!("failed to spawn shell: {e}")),
    }
}

fn shell_list(background: Option<&BackgroundProcessManager>) -> ToolOutcome {
    let Some(background) = background else {
        return ToolOutcome::error("background shell execution is not available");
    };
    let snapshots = background.snapshots();
    if snapshots.is_empty() {
        return ToolOutcome::ok("no background shell commands are tracked".into());
    }
    let now = std::time::Instant::now();
    let mut output = String::new();
    for snapshot in snapshots {
        let pid = snapshot
            .pid
            .map_or_else(|| "?".into(), |pid| pid.to_string());
        output.push_str(&format!(
            "{}  {}  pid={}  elapsed={}s  {}  command={}\n",
            snapshot.id,
            snapshot.status.detail(),
            pid,
            snapshot.elapsed(now).as_secs(),
            snapshot.name.as_deref().unwrap_or("unnamed"),
            snapshot.command
        ));
    }
    ToolOutcome::ok(output.trim_end().to_string())
}

fn shell_output(background: Option<&BackgroundProcessManager>, args: &Value) -> ToolOutcome {
    let Some(background) = background else {
        return ToolOutcome::error("background shell execution is not available");
    };
    let id = match str_arg(args, "id") {
        Ok(id) => id,
        Err(error) => return error,
    };
    let cursor = match args.get("cursor") {
        Some(value) => match value.as_u64() {
            Some(cursor) => cursor,
            None => return ToolOutcome::error("'cursor' must be a non-negative integer"),
        },
        None => 0,
    };
    let wait_secs = match args.get("wait_secs") {
        Some(value) => match value.as_u64() {
            Some(wait) => wait,
            None => return ToolOutcome::error("'wait_secs' must be a non-negative integer"),
        },
        None => 0,
    };
    if wait_secs > 30 {
        return ToolOutcome::error("'wait_secs' must be between 0 and 30");
    }
    match background.read_output(id, cursor, Duration::from_secs(wait_secs)) {
        Ok(read) => ToolOutcome::ok(format_background_output(read)),
        Err(error) => ToolOutcome::error(error),
    }
}

fn format_background_output(read: OutputRead) -> String {
    let mut output = format!(
        "{}: {} (pid {})\n",
        read.snapshot.id,
        read.snapshot.status.detail(),
        read.snapshot
            .pid
            .map_or_else(|| "?".into(), |pid| pid.to_string())
    );
    if read.stale_cursor {
        output.push_str("[earlier output was discarded]\n");
    }
    let mut previous = None;
    for chunk in read.chunks {
        if previous != Some(chunk.stream) {
            if !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&format!("[{}]\n", chunk.stream.label()));
            previous = Some(chunk.stream);
        }
        output.push_str(&chunk.text);
    }
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&format!("next_cursor: {}", read.next_cursor));
    output
}

fn shell_stop(background: Option<&BackgroundProcessManager>, args: &Value) -> ToolOutcome {
    let Some(background) = background else {
        return ToolOutcome::error("background shell execution is not available");
    };
    let id = match str_arg(args, "id") {
        Ok(id) => id,
        Err(error) => return error,
    };
    match background.stop(id) {
        Ok(snapshot) if snapshot.status.is_active() => {
            ToolOutcome::ok(format!("stop requested for {id}"))
        }
        Ok(snapshot) => ToolOutcome::ok(format!("{id}: {}", snapshot.status.detail())),
        Err(error) => ToolOutcome::error(error),
    }
}

#[cfg(test)]
fn read_file(args: &Value) -> ToolOutcome {
    read_file_for_model(args, false)
}

fn read_file_for_model(args: &Value, supports_images: bool) -> ToolOutcome {
    let path = match str_arg(args, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::fs::File::open(path) {
        Ok(file) => read_bounded_file(path, file, supports_images),
        Err(e) => ToolOutcome::error(format!("cannot read {path}: {e}")),
    }
}

fn read_bounded_file(path: &str, mut reader: impl Read, supports_images: bool) -> ToolOutcome {
    const IMAGE_SIGNATURE_BYTES: usize = 12;
    let mut prefix = Vec::with_capacity(IMAGE_SIGNATURE_BYTES);
    if let Err(error) = reader.by_ref().take(12).read_to_end(&mut prefix) {
        return ToolOutcome::error(format!("cannot read {path}: {error}"));
    }
    let media_type = crate::image::media_type(&prefix);
    let reader = std::io::Cursor::new(prefix).chain(reader);
    let Some(media_type) = media_type else {
        return read_bounded_utf8(path, reader);
    };
    if !supports_images {
        return ToolOutcome::error("the selected model does not accept image input");
    }

    let mut bytes = Vec::new();
    if let Err(error) = reader
        .take(crate::image::MAX_IMAGE_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
    {
        return ToolOutcome::error(format!("cannot read {path}: {error}"));
    }
    if bytes.len() > crate::image::MAX_IMAGE_BYTES {
        return ToolOutcome::error(format!(
            "{path} exceeds the {}-byte image limit",
            crate::image::MAX_IMAGE_BYTES
        ));
    }
    let size = bytes.len();
    ToolOutcome::image(
        format!("read {media_type} image from {path} ({size} bytes)"),
        crate::image::encode(media_type, &bytes),
    )
}

fn read_skill(skills: &[Skill], args: &Value) -> ToolOutcome {
    let name = match str_arg(args, "name") {
        Ok(name) => name,
        Err(error) => return error,
    };
    let Some(skill) = skills.iter().find(|skill| skill.name == name) else {
        return ToolOutcome::error(format!(
            "skill '{name}' is not available for model invocation"
        ));
    };
    ToolOutcome::ok(crate::skills::tool_result(skill))
}

fn read_bounded_utf8(path: &str, reader: impl Read) -> ToolOutcome {
    let mut bytes = Vec::new();
    if let Err(error) = reader
        .take(MAX_READ_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
    {
        return ToolOutcome::error(format!("cannot read {path}: {error}"));
    }
    if bytes.len() > MAX_READ_FILE_BYTES as usize {
        return ToolOutcome::error(format!(
            "{path} exceeds the {MAX_READ_FILE_BYTES}-byte read limit; \
             use shell tools to read portions"
        ));
    }
    match String::from_utf8(bytes) {
        Ok(text) => ToolOutcome::ok(text),
        Err(_) => ToolOutcome::error(format!("{path} is not valid UTF-8 (binary file?)")),
    }
}

fn write_file(args: &Value) -> ToolOutcome {
    let path = match str_arg(args, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let content = match str_arg(args, "content") {
        Ok(c) => c,
        Err(e) => return e,
    };
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return ToolOutcome::error(format!("cannot create {}: {e}", parent.display()));
    }
    match std::fs::write(path, content) {
        Ok(()) => ToolOutcome::ok(format!("wrote {} bytes to {path}", content.len())),
        Err(e) => ToolOutcome::error(format!("cannot write {path}: {e}")),
    }
}

fn edit_file(args: &Value) -> ToolOutcome {
    let path = match str_arg(args, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let old_string = match str_arg(args, "old_string") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let new_string = match str_arg(args, "new_string") {
        Ok(s) => s,
        Err(e) => return e,
    };
    if old_string.is_empty() {
        return ToolOutcome::error("old_string must not be empty");
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return ToolOutcome::error(format!("cannot read {path}: {e}")),
    };
    let count = text.matches(old_string).count();
    match count {
        0 => ToolOutcome::error(format!("old_string not found in {path}")),
        1 => {
            let updated = text.replacen(old_string, new_string, 1);
            match std::fs::write(path, updated) {
                Ok(()) => ToolOutcome::ok(format!("edited {path}")),
                Err(e) => ToolOutcome::error(format!("cannot write {path}: {e}")),
            }
        }
        n => ToolOutcome::error(format!(
            "old_string appears {n} times in {path}; add surrounding context to make it unique"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    use crate::background::{
        BackgroundId, BackgroundSnapshot, BackgroundStatus, LogChunk, OutputStream,
    };

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("yawl-tools-{}-{name}", std::process::id()))
    }

    fn registry_config(home_dir: std::path::PathBuf, project_dir: std::path::PathBuf) -> Config {
        Config {
            model: Some("test".into()),
            max_tokens: 1,
            home_dir,
            project_dir,
            ..Config::test_default()
        }
    }

    #[test]
    fn edit_file_requires_unique_match() -> std::io::Result<()> {
        let path = temp_path("edit.txt");
        std::fs::write(&path, "aaa bbb aaa")?;
        let p = path.to_string_lossy();

        let dup = edit_file(&json!({"path": &p, "old_string": "aaa", "new_string": "x"}));
        assert!(dup.is_error);
        assert!(dup.content.contains("2 times"));

        let ok = edit_file(&json!({"path": &p, "old_string": "bbb", "new_string": "yyy"}));
        assert!(!ok.is_error);
        assert_eq!(std::fs::read_to_string(&path)?, "aaa yyy aaa");
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[test]
    fn write_file_creates_parent_dirs() -> std::io::Result<()> {
        let dir = temp_path("nested");
        let file = dir.join("a/b.txt");
        let out = write_file(&json!({"path": file.to_string_lossy(), "content": "hi"}));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&file)?, "hi");
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn shell_builtin_reports_exit_code() {
        let out = shell(&json!({"command": "echo hello; exit 2"}), None);
        assert!(out.is_error);
        assert!(out.content.contains("hello"));
        assert!(out.content.contains("exit code: 2"));
    }

    #[test]
    fn web_tools_are_advertised_only_when_enabled() {
        let mut config = registry_config(temp_path("web-home"), temp_path("web-project"));
        let mut cache = DescribeCache::default();
        let disabled = Registry::scan(&config, &mut cache);
        assert!(
            disabled
                .specs()
                .iter()
                .all(|spec| !spec.name.starts_with("web_"))
        );
        assert!(!disabled.has_web_tools());

        config.web_browsing = true;
        let mut enabled = Registry::scan(&config, &mut cache);
        assert!(enabled.has_web_tools());
        let names = enabled
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"web_search".to_string()));
        assert!(names.contains(&"web_fetch".to_string()));

        enabled.retain_names(&["web_fetch".into()]);
        assert_eq!(enabled.specs()[0].name, "web_fetch");
        assert!(enabled.has_web_tools());

        enabled.retain_names(&["read_file".into()]);
        assert!(!enabled.has_web_tools());
    }

    #[test]
    fn executable_web_names_are_reserved_only_when_builtins_are_enabled() -> std::io::Result<()> {
        let root = temp_path("conditional-web-reservation");
        let home_dir = root.join("home");
        let project_dir = root.join("project");
        let tools_dir = project_dir.join("tools");
        std::fs::create_dir_all(&tools_dir)?;
        let tool_path = tools_dir.join("custom-web-search");
        std::fs::write(
            &tool_path,
            r#"#!/bin/sh
if [ "$1" = "--describe" ]; then
  echo '{"name":"web_search","description":"custom search","input_schema":{"type":"object"}}'
else
  echo custom
fi
"#,
        )?;
        let mut permissions = std::fs::metadata(&tool_path)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&tool_path, permissions)?;

        let mut config = registry_config(home_dir, project_dir);
        let mut cache = DescribeCache::default();
        let disabled = Registry::scan(&config, &mut cache);
        assert!(
            disabled.describe_all().iter().any(
                |(name, description, _)| name == "web_search" && description == "custom search"
            )
        );

        config.web_browsing = true;
        let enabled = Registry::scan(&config, &mut cache);
        assert!(
            enabled.describe_all().iter().any(
                |(name, description, _)| name == "web_search" && description != "custom search"
            )
        );
        assert!(
            enabled
                .warnings
                .iter()
                .any(|warning| warning.contains("reserved"))
        );
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn background_output_keeps_adjacent_read_chunks_contiguous() {
        let now = std::time::Instant::now();
        let output = format_background_output(OutputRead {
            snapshot: BackgroundSnapshot {
                id: BackgroundId::new(1),
                pid: Some(42),
                command: "printf hello".into(),
                name: None,
                cwd: std::path::PathBuf::from("."),
                timeout: None,
                status: BackgroundStatus::Running,
                started_at: now,
                settled_at: None,
            },
            chunks: vec![
                LogChunk {
                    cursor: 0,
                    stream: OutputStream::Stdout,
                    text: "hello ".into(),
                },
                LogChunk {
                    cursor: 1,
                    stream: OutputStream::Stdout,
                    text: "world".into(),
                },
            ],
            stale_cursor: false,
            next_cursor: 2,
        });

        assert!(output.contains("[stdout]\nhello world\nnext_cursor: 2"));
        assert!(!output.contains("hello \nworld"));
    }

    #[test]
    fn background_tools_are_main_agent_only() {
        let root = temp_path("background-registry");
        let config = registry_config(root.join("home"), root.join("project"));
        let mut cache = DescribeCache::default();
        let child = Registry::scan(&config, &mut cache);
        let child_names = child
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert!(!child_names.iter().any(|name| name == "shell_output"));
        let child_shell = child
            .specs()
            .into_iter()
            .find(|spec| spec.name == "shell")
            .expect("child shell tool");
        assert!(
            child_shell.input_schema["properties"]
                .get("background")
                .is_none()
        );

        let manager = BackgroundProcessManager::default();
        let main = Registry::scan_with_background(&config, &mut cache, manager.clone());
        let main_names = main
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert!(
            ["shell_list", "shell_output", "shell_stop"]
                .iter()
                .all(|name| main_names.iter().any(|candidate| candidate == name))
        );
        let main_shell = main
            .specs()
            .into_iter()
            .find(|spec| spec.name == "shell")
            .expect("main shell tool");
        assert!(
            main_shell.input_schema["properties"]
                .get("background")
                .is_some()
        );
        manager.shutdown_and_discard();
    }

    #[test]
    fn background_shell_can_be_read_listed_and_stopped() {
        let root = temp_path("background-tools");
        let config = registry_config(root.join("home"), root.join("project"));
        let manager = BackgroundProcessManager::default();
        let registry =
            Registry::scan_with_background(&config, &mut DescribeCache::default(), manager.clone());
        let started = registry.execute(
            "shell",
            r#"{"command":"printf ready; trap 'exit 0' TERM; while :; do sleep 1; done","background":true,"name":"dev"}"#,
            "session",
        );
        assert!(!started.is_error, "{}", started.content);
        assert!(started.content.contains("bg-1"));
        let output = registry.execute(
            "shell_output",
            r#"{"id":"bg-1","cursor":0,"wait_secs":2}"#,
            "session",
        );
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("ready"));
        assert!(output.content.contains("next_cursor:"));
        let listed = registry.execute("shell_list", "{}", "session");
        assert!(listed.content.contains("dev"));
        let stopped = registry.execute("shell_stop", r#"{"id":"bg-1"}"#, "session");
        assert!(!stopped.is_error, "{}", stopped.content);
        manager.shutdown_and_discard();
    }

    #[test]
    fn registry_discovers_and_invokes_exec_tool() -> std::io::Result<()> {
        let root = temp_path("exec-registry");
        let home_dir = root.join("home");
        let project_dir = root.join("project");
        let tools_dir = project_dir.join("tools");
        std::fs::create_dir_all(&tools_dir)?;
        let tool_path = tools_dir.join("echo_session");
        std::fs::write(
            &tool_path,
            r#"#!/bin/sh
if [ "$1" = "--describe" ]; then
  echo '{"name":"echo_session","description":"test tool","input_schema":{"type":"object"}}'
  exit 0
fi
input=$(cat)
printf '%s:%s' "$YAWL_SESSION_ID" "$input"
"#,
        )?;
        let mut permissions = std::fs::metadata(&tool_path)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&tool_path, permissions)?;

        let config = registry_config(home_dir, project_dir);
        let mut cache = DescribeCache::default();
        let registry = Registry::scan(&config, &mut cache);
        assert!(
            registry
                .describe_all()
                .iter()
                .any(|(name, _, _)| name == "echo_session")
        );
        let outcome = registry.execute("echo_session", r#"{"value":1}"#, "session-7");
        assert!(!outcome.is_error, "{}", outcome.content);
        assert_eq!(outcome.content, r#"session-7:{"value":1}"#);
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn subagent_tools_are_conditional_and_reserved() -> std::io::Result<()> {
        let root = temp_path("reserved-subagent-tool");
        let home_dir = root.join("home");
        let project_dir = root.join("project");
        let tools_dir = project_dir.join("tools");
        std::fs::create_dir_all(&tools_dir)?;
        let tool_path = tools_dir.join("reserved");
        std::fs::write(
            &tool_path,
            r#"#!/bin/sh
if [ "$1" = "--describe" ]; then
  echo '{"name":"subagent_spawn","description":"override","input_schema":{"type":"object"}}'
fi
"#,
        )?;
        let mut permissions = std::fs::metadata(&tool_path)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&tool_path, permissions)?;
        let mut config = registry_config(home_dir, project_dir);
        let mut cache = DescribeCache::default();

        let disabled = Registry::scan(&config, &mut cache);
        assert!(
            disabled
                .specs()
                .iter()
                .all(|spec| !spec.name.starts_with("subagent_"))
        );
        assert!(
            disabled
                .warnings
                .iter()
                .any(|warning| warning.contains("reserved"))
        );

        config.subagents = true;
        let manager = SubagentManager::new("session".into(), config.max_subagents);
        let enabled = Registry::scan_with_subagents(&config, &mut cache, manager, "test");
        let names = enabled
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert!(
            RESERVED_TOOL_NAMES
                .iter()
                .filter(|name| name.starts_with("subagent_"))
                .all(|name| names.iter().any(|candidate| candidate == name))
        );
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn spawn_tool_lists_presets_and_takes_an_optional_agent() {
        let root = temp_path("preset-spawn-tool");
        let config = registry_config(root.join("home"), root.join("project"));
        let manager = SubagentManager::new("session".into(), config.max_subagents);
        let registry =
            Registry::scan_with_subagents(&config, &mut DescribeCache::default(), manager, "test");

        let specs = registry.specs();
        let orchestration_chars = specs
            .iter()
            .filter(|spec| spec.name.starts_with("subagent_"))
            .map(|spec| spec.description.len() + spec.input_schema.to_string().len())
            .sum::<usize>();
        assert!(
            orchestration_chars < 2_000,
            "orchestration schemas should stay compact; got {orchestration_chars} bytes"
        );
        let spawn = specs
            .into_iter()
            .find(|spec| spec.name == "subagent_spawn")
            .expect("spawn tool present");
        assert!(
            spawn.description.contains("scout (read_file+read_skill)"),
            "the description should advertise bundled presets; got:\n{}",
            spawn.description
        );
        assert!(spawn.description.contains("# Target"));
        let required = spawn.input_schema.get("required").expect("required list");
        assert_eq!(
            required,
            &json!(["prompt", "required_tools"]),
            "name and agent are optional, but capability planning is required"
        );
        assert!(
            spawn
                .description
                .contains("omit agent for writes, commands")
        );
        let properties = spawn
            .input_schema
            .get("properties")
            .expect("properties object");
        assert!(properties.get("agent").is_some());
        assert!(properties.get("required_tools").is_some());
        assert!(
            properties.get("model").is_none(),
            "the orchestrator must not override the configured child model"
        );
        assert!(
            properties["agent"]["description"]
                .as_str()
                .is_some_and(|text| text.contains("Scout is read-only"))
        );
    }

    #[test]
    fn spawn_without_model_inherits_the_active_parent() {
        let root = temp_path("spawn-model-inherit");
        let mut config = registry_config(root.join("home"), root.join("project"));
        config.openai_base_url = "http://127.0.0.1:9/v1".into();
        let manager = SubagentManager::new("session".into(), config.max_subagents);
        let registry = Registry::scan_with_subagents(
            &config,
            &mut DescribeCache::default(),
            manager.clone(),
            "openai:active-parent",
        );
        let args = json!({
            "prompt": "Report the delegated result directly.",
            "required_tools": []
        });

        let outcome = registry.execute("subagent_spawn", &args.to_string(), "session");

        assert!(!outcome.is_error, "unexpected outcome: {}", outcome.content);
        assert_eq!(manager.snapshots()[0].model, "openai:active-parent");
        manager.shutdown_and_discard();
    }

    #[test]
    fn spawn_rejects_model_overrides_from_the_orchestrator() {
        let root = temp_path("spawn-model-override");
        let config = registry_config(root.join("home"), root.join("project"));
        let manager = SubagentManager::new("session".into(), config.max_subagents);
        let registry = Registry::scan_with_subagents(
            &config,
            &mut DescribeCache::default(),
            manager.clone(),
            "openai-codex:parent",
        );
        let args = json!({
            "prompt": "Inspect the requested file.",
            "required_tools": ["read_file"],
            "model": "gpt-4.1-mini"
        });

        let outcome = registry.execute("subagent_spawn", &args.to_string(), "session");

        assert!(outcome.is_error, "unexpected outcome: {}", outcome.content);
        assert!(
            outcome.content.contains("not accepted"),
            "{}",
            outcome.content
        );
        assert!(manager.snapshots().is_empty());
        manager.shutdown_and_discard();
    }

    #[test]
    fn scout_rejects_spawn_tasks_that_require_file_writes() {
        let root = temp_path("scout-write-capability");
        let config = registry_config(root.join("home"), root.join("project"));
        let manager = SubagentManager::new("session".into(), config.max_subagents);
        let registry = Registry::scan_with_subagents(
            &config,
            &mut DescribeCache::default(),
            manager.clone(),
            "test",
        );
        let args = json!({
            "agent": "scout",
            "prompt": "Create short_story.txt and write a story into it.",
            "required_tools": ["write_file"]
        });

        let outcome = registry.execute("subagent_spawn", &args.to_string(), "session");

        assert!(outcome.is_error, "unexpected outcome: {}", outcome.content);
        assert!(outcome.content.contains("scout"), "{}", outcome.content);
        assert!(
            outcome.content.contains("write_file"),
            "{}",
            outcome.content
        );
        manager.shutdown_and_discard();
    }

    #[test]
    fn retain_names_filters_the_registry_for_preset_children() {
        let root = temp_path("preset-allowlist");
        let config = registry_config(root.join("home"), root.join("project"));
        let mut registry = Registry::scan(&config, &mut DescribeCache::default());

        registry.retain_names(&["read_file".to_string(), "shell".to_string()]);

        let mut names = registry
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, ["read_file", "shell"]);
    }

    #[test]
    fn planning_registry_gates_shell_to_read_only_inspection() {
        let root = temp_path("planning-registry");
        let mut config = registry_config(root.join("home"), root.join("project"));
        config.web_browsing = true;
        let manager = SubagentManager::new("session".into(), config.max_subagents);
        let mut registry = Registry::scan_with_subagents_and_background(
            &config,
            &mut DescribeCache::default(),
            manager.clone(),
            "test",
            BackgroundProcessManager::default(),
        );
        let broker = QuestionBroker::default();
        broker.enable();
        registry.advertise_user_input(broker);

        registry.retain_for_planning();
        registry.advertise_plan_complete();

        let names = registry
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<std::collections::HashSet<_>>();
        for required in [
            "shell",
            "read_file",
            "web_search",
            "web_fetch",
            USER_INPUT_TOOL_NAME,
            PLAN_COMPLETE_TOOL_NAME,
        ] {
            assert!(names.contains(required), "missing planning tool {required}");
        }
        for forbidden in [
            "write_file",
            "edit_file",
            "shell_list",
            "shell_output",
            "shell_stop",
            "subagent_spawn",
            "subagent_wait",
        ] {
            assert!(
                !names.contains(forbidden),
                "planning exposed forbidden tool {forbidden}"
            );
        }
        let inspection = registry.execute(
            "shell",
            r#"{"command":"rg --files | head -n 1"}"#,
            "session",
        );
        assert!(!inspection.is_error, "{}", inspection.content);
        let git = registry.execute("shell", r#"{"command":"git status --short"}"#, "session");
        assert!(!git.is_error, "{}", git.content);
        let mutation = registry.execute(
            "shell",
            r#"{"command":"printf nope > changed.txt"}"#,
            "session",
        );
        assert!(mutation.is_error);
        let git_mutation =
            registry.execute("shell", r#"{"command":"git reset --hard"}"#, "session");
        assert!(git_mutation.is_error);
        assert!(!root.join("project/changed.txt").exists());
        manager.shutdown_and_discard();
    }

    #[test]
    fn registry_exposes_only_model_invokable_skills() -> std::io::Result<()> {
        let root = temp_path("skill-registry");
        let skills = root.join("skills");
        std::fs::create_dir_all(skills.join("automatic"))?;
        std::fs::create_dir_all(skills.join("manual"))?;
        std::fs::write(
            skills.join("automatic/SKILL.md"),
            "---\nname: automatic\ndescription: Use automatically\n---\nRead all relevant code.\n",
        )?;
        std::fs::write(
            skills.join("manual/SKILL.md"),
            "---\nname: manual\ndescription: Use manually\ndisable-model-invocation: true\n---\nOnly when requested.\n",
        )?;
        let mut config = registry_config(root.join("home"), root.join("project"));
        config.skill_dirs = vec![skills.clone()];
        config.global_skill_dirs = vec![skills];

        let mut registry = Registry::scan(&config, &mut DescribeCache::default());
        assert_eq!(
            registry
                .skills()
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            ["automatic"]
        );
        assert!(
            registry
                .specs()
                .iter()
                .any(|spec| spec.name == "read_skill")
        );
        let loaded = registry.execute("read_skill", r#"{"name":"automatic"}"#, "session");
        assert!(!loaded.is_error, "{}", loaded.content);
        assert!(loaded.content.contains("Read all relevant code."));
        assert!(loaded.content.contains(&root.display().to_string()));
        let denied = registry.execute("read_skill", r#"{"name":"manual"}"#, "session");
        assert!(denied.is_error);

        registry.retain_names(&["read_file".into()]);
        assert!(registry.skills().is_empty());
        assert!(
            registry
                .specs()
                .iter()
                .all(|spec| spec.name != "read_skill")
        );
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn read_file_rejects_files_over_the_size_limit() -> std::io::Result<()> {
        let path = temp_path("large.bin");
        std::fs::write(&path, vec![b'a'; MAX_READ_FILE_BYTES as usize + 1])?;

        let out = read_file(&json!({"path": path.to_string_lossy()}));

        assert!(out.is_error);
        assert!(out.content.contains("read limit"));
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[test]
    fn read_file_bounds_streams_without_relying_on_metadata() {
        let out = read_bounded_utf8("endless", std::io::repeat(b'a'));

        assert!(out.is_error);
        assert!(out.content.contains("read limit"));
    }

    #[test]
    fn read_file_keeps_non_images_at_the_text_read_limit() {
        struct CountingRepeat(std::rc::Rc<std::cell::Cell<usize>>);

        impl Read for CountingRepeat {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                buffer.fill(b'a');
                self.0.set(self.0.get().saturating_add(buffer.len()));
                Ok(buffer.len())
            }
        }

        let bytes_read = std::rc::Rc::new(std::cell::Cell::new(0));
        let out = read_bounded_file("endless", CountingRepeat(bytes_read.clone()), false);

        assert!(out.is_error);
        assert!(out.content.contains("read limit"));
        assert_eq!(
            bytes_read.get(),
            MAX_READ_FILE_BYTES as usize + 1,
            "text detection must not read up to the larger image limit"
        );
    }

    #[test]
    fn read_file_returns_images_only_for_capable_models() -> std::io::Result<()> {
        let path = temp_path("read-image.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\npayload")?;
        let args = json!({"path": path.to_string_lossy()});

        let supported = read_file_for_model(&args, true);
        assert!(!supported.is_error, "{}", supported.content);
        assert_eq!(supported.images.len(), 1);
        assert_eq!(supported.images[0].media_type, "image/png");

        let unsupported = read_file_for_model(&args, false);
        assert!(unsupported.is_error);
        assert!(unsupported.images.is_empty());
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn result_truncation_preserves_utf8_boundaries() {
        let exact = "é".repeat(MAX_RESULT_CHARS);
        let mut unchanged = exact.clone();
        truncate_result(&mut unchanged);
        assert_eq!(unchanged, exact);

        let mut truncated = "é".repeat(MAX_RESULT_CHARS + 1);
        truncate_result(&mut truncated);
        assert!(truncated.ends_with("\n[output truncated]"));
        assert_eq!(truncated.matches('é').count(), MAX_RESULT_CHARS);
    }
}
