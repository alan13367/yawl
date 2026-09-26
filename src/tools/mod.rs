//! Tool registry: builtins plus exec tools discovered from disk.
//!
//! The registry is rescanned every agent-loop iteration, so a tool the model
//! just wrote is usable on its next turn. On name collisions the last scan
//! wins (builtins < `~/.yawl/tools` < `./.yawl/tools`).

mod catalog;
pub mod exec;
mod files;
mod git;
mod mode;
mod orchestration;
mod output;
mod planning_shell;
mod shell;
mod skills;
mod user_input;
mod web;

use std::sync::{Arc, OnceLock};

use serde_json::Value;

use crate::background::BackgroundProcessManager;
use crate::config::Config;
use crate::provider::ToolSpec;
use crate::skills::Skill;
use crate::subagent::SubagentManager;

pub use catalog::CatalogCache;
pub use exec::DescribeCache;
pub(crate) use user_input::{QuestionBroker, QuestionSnapshot};
#[cfg(test)]
pub(crate) use user_input::{QuestionOption, UserQuestion};

/// Cap on tool result size fed back to the model.
const MAX_RESULT_CHARS: usize = 60_000;

enum ToolImpl {
    Shell,
    ShellList,
    ShellOutput,
    ShellStop,
    ReadFile,
    ListFiles,
    SearchFiles,
    GitInspect,
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
    Subagent(orchestration::SubagentTool),
}

struct ToolEntry {
    spec: Arc<ToolSpec>,
    imp: ToolImpl,
    tokens: OnceLock<u64>,
}

impl ToolEntry {
    fn new(spec: ToolSpec, imp: ToolImpl) -> Self {
        Self {
            spec: Arc::new(spec),
            imp,
            tokens: OnceLock::new(),
        }
    }

    /// Estimated prompt tokens this entry contributes, computed once per
    /// entry so repeated scans do not re-serialize schemas.
    fn prompt_tokens(&self) -> u64 {
        *self.tokens.get_or_init(|| spec_tokens(&self.spec))
    }
}

/// Mirrors the byte-based prompt estimate in
/// `agent::conversation::context`, for one tool spec.
fn spec_tokens(spec: &ToolSpec) -> u64 {
    fn text_tokens(text: &str) -> u64 {
        (text.len() as u64).div_ceil(3)
    }
    text_tokens(&spec.name)
        .saturating_add(text_tokens(&spec.description))
        .saturating_add(text_tokens(&spec.input_schema.to_string()))
        .saturating_add(16)
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
    output_directory: std::path::PathBuf,
    entries: Vec<Arc<ToolEntry>>,
    pub warnings: Vec<String>,
    skills: Arc<Vec<Skill>>,
    subagents: Option<orchestration::SubagentContext>,
    background: Option<BackgroundProcessManager>,
    web: Option<web::WebTools>,
}

impl Registry {
    /// Scans builtins + exec tool directories. Called every loop iteration;
    /// `cache` reuses unchanged filesystem catalogs and `--describe` results.
    pub fn scan(config: &Config, cache: &mut CatalogCache) -> Registry {
        Self::scan_inner(config, cache, None)
    }

    /// Restricted children can opt into native file discovery through their
    /// preset allowlist. Main agents and unrestricted children use shell.
    pub(crate) fn scan_for_child(
        config: &Config,
        cache: &mut CatalogCache,
        allowlist: Option<&[String]>,
    ) -> Registry {
        let mut registry = Self::scan(config, cache);
        if let Some(allowed) = allowlist {
            registry
                .entries
                .extend(files::entries().into_iter().map(Arc::new));
            registry.entries.push(Arc::new(git::entry()));
            registry.retain_names(allowed);
        }
        registry
    }

    /// Scans the tools advertised by the persistent main agent without
    /// creating a live process manager. Used by `yawl --list-tools`.
    pub fn scan_for_main_listing(config: &Config, cache: &mut CatalogCache) -> Registry {
        let mut registry = Self::scan(config, cache);
        if let Some(shell) = registry
            .entries
            .iter_mut()
            .find(|entry| entry.spec.name == "shell" && matches!(&entry.imp, ToolImpl::Shell))
        {
            *shell = Arc::new(shell::entry(true));
        }
        registry
            .entries
            .extend(shell::background_entries().into_iter().map(Arc::new));
        registry
    }

    fn scan_inner(
        config: &Config,
        cache: &mut CatalogCache,
        background: Option<BackgroundProcessManager>,
    ) -> Registry {
        let mut registry = Registry {
            output_directory: config.home_dir.join("artifacts/tool-output"),
            entries: cache.builtins(config, background.is_some()),
            warnings: Vec::new(),
            skills: Arc::default(),
            subagents: None,
            background,
            web: config.web_browsing.then(|| web::WebTools::new(config)),
        };
        let (skills, warnings) = cache.skills(config);
        registry.warnings.extend(warnings);
        registry.skills = skills;
        if !registry.skills.is_empty() {
            registry.insert(skills::entry());
        }
        for dir in config.tool_dirs() {
            let (tools, warnings) = exec::scan_dir(&dir, cache.describe());
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
                registry.insert(ToolEntry::new(tool.spec.clone(), ToolImpl::Exec(tool)));
            }
        }
        registry
    }

    #[cfg(test)]
    pub(crate) fn scan_with_subagents(
        config: &Config,
        cache: &mut CatalogCache,
        manager: SubagentManager,
        parent_model: &str,
    ) -> Registry {
        let mut registry = Self::scan(config, cache);
        registry.enable_subagents(config, cache, manager, parent_model);
        registry
    }

    pub(crate) fn scan_with_background(
        config: &Config,
        cache: &mut CatalogCache,
        background: BackgroundProcessManager,
    ) -> Registry {
        Self::scan_inner(config, cache, Some(background))
    }

    pub(crate) fn scan_with_subagents_and_background(
        config: &Config,
        cache: &mut CatalogCache,
        manager: SubagentManager,
        parent_model: &str,
        background: BackgroundProcessManager,
    ) -> Registry {
        let mut registry = Self::scan_inner(config, cache, Some(background));
        registry.enable_subagents(config, cache, manager, parent_model);
        registry
    }

    /// Drops every entry whose name is not listed. Used for preset
    /// subagents, whose tool allowlist is enforced at scan time.
    pub(crate) fn retain_names(&mut self, names: &[String]) {
        self.entries
            .retain(|entry| names.contains(&entry.spec.name));
        if !names.iter().any(|name| name == "read_skill") {
            self.skills = Arc::default();
        }
    }

    fn insert(&mut self, entry: ToolEntry) {
        let entry = Arc::new(entry);
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

    pub fn specs(&self) -> Vec<Arc<ToolSpec>> {
        self.entries.iter().map(|e| Arc::clone(&e.spec)).collect()
    }

    /// Total estimated prompt cost of the advertised tools. Entries cached
    /// across scans compute this once instead of re-serializing schemas.
    pub(crate) fn tool_tokens(&self) -> u64 {
        self.entries.iter().fold(0, |tokens, entry| {
            tokens.saturating_add(entry.prompt_tokens())
        })
    }

    pub(crate) fn advertise_goal_complete(&mut self) {
        self.insert(mode::goal_complete_entry());
    }

    pub(crate) fn advertise_user_input(&mut self, broker: QuestionBroker) {
        self.insert(user_input::entry(broker));
    }

    pub(crate) fn advertise_plan_complete(&mut self) {
        self.insert(mode::plan_complete_entry());
    }

    pub(crate) fn advertise_plan_action(&mut self) {
        self.insert(mode::plan_action_entry());
    }

    pub(crate) fn advertise_plan_implemented(&mut self) {
        self.insert(mode::plan_implemented_entry());
    }

    /// Keeps only read tools and replaces the unrestricted shell with its
    /// planning-only inspection gate.
    pub(crate) fn retain_for_planning(&mut self) {
        self.entries.retain(|entry| {
            matches!(
                &entry.imp,
                ToolImpl::Shell
                    | ToolImpl::ReadFile
                    | ToolImpl::ListFiles
                    | ToolImpl::SearchFiles
                    | ToolImpl::GitInspect
                    | ToolImpl::ReadSkill
                    | ToolImpl::WebSearch
                    | ToolImpl::WebFetch
                    | ToolImpl::UserInput(_)
                    | ToolImpl::PlanComplete
            )
        });
        self.insert(planning_shell::entry());
    }

    pub(crate) fn skills(&self) -> &[Skill] {
        self.skills.as_slice()
    }

    pub(crate) fn has_web_tools(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(&entry.imp, ToolImpl::WebSearch | ToolImpl::WebFetch))
    }

    pub(crate) fn has_subagent_tools(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(&entry.imp, ToolImpl::Subagent(_)))
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
            ToolImpl::Shell => shell::execute(&args, self.background.as_ref()),
            ToolImpl::ShellList => shell::list(self.background.as_ref()),
            ToolImpl::ShellOutput => shell::output(self.background.as_ref(), &args),
            ToolImpl::ShellStop => shell::stop(self.background.as_ref(), &args),
            ToolImpl::ReadFile => files::read_file_for_model(&args, supports_images),
            ToolImpl::ListFiles => files::discover(&args, false),
            ToolImpl::SearchFiles => files::discover(&args, true),
            ToolImpl::GitInspect => git::inspect(&args),
            ToolImpl::ReadSkill => skills::read(&self.skills, &args),
            ToolImpl::WriteFile => files::write_file(&args),
            ToolImpl::EditFile => files::edit_file(&args),
            ToolImpl::WebSearch => web::execute(self.web.as_ref(), &args, true),
            ToolImpl::WebFetch => web::execute(self.web.as_ref(), &args, false),
            ToolImpl::GoalComplete => mode::non_empty_arg(&args, "result", GOAL_COMPLETE_TOOL_NAME),
            ToolImpl::PlanningShell => match planning_shell::prepare(&args) {
                Ok(args) => shell::execute_with_path(
                    &args,
                    None,
                    planning_shell::inspection_path().as_deref(),
                ),
                Err(error) => ToolOutcome::error(error),
            },
            ToolImpl::UserInput(broker) => match user_input::parse_questions(&args)
                .and_then(|questions| broker.ask(questions))
            {
                Ok(content) => ToolOutcome::ok(content),
                Err(error) => ToolOutcome::error(error),
            },
            ToolImpl::PlanComplete => mode::non_empty_arg(&args, "plan", PLAN_COMPLETE_TOOL_NAME),
            ToolImpl::PlanAction => mode::plan_action_outcome(&args),
            ToolImpl::PlanImplemented => {
                mode::non_empty_arg(&args, "result", PLAN_IMPLEMENTED_TOOL_NAME)
            }
            ToolImpl::Exec(tool) => {
                let (content, is_error) = exec::invoke(tool, args_json, session_id);
                ToolOutcome {
                    content,
                    images: Vec::new(),
                    is_error,
                }
            }
            ToolImpl::Subagent(tool) => {
                orchestration::execute(self.subagents.as_ref(), *tool, &args)
            }
        };
        // `web_fetch` returns up to its configured limit inline and saves
        // longer pages itself.
        let save_output = matches!(entry.imp, ToolImpl::Exec(_) | ToolImpl::PlanningShell)
            || (matches!(entry.imp, ToolImpl::Shell)
                && args.get("background").and_then(Value::as_bool) != Some(true));
        if save_output {
            output::prepare(&self.output_directory, &mut outcome.content);
        }
        // User answers are requirements, not disposable command output.
        if !matches!(entry.imp, ToolImpl::UserInput(_)) {
            truncate_result(&mut outcome.content);
        }
        if outcome.content.is_empty() {
            outcome.content = if outcome.is_error {
                "(no output)".to_string()
            } else {
                "(no output; command succeeded)".to_string()
            };
        }
        outcome
    }
}

const RESERVED_TOOL_NAMES: &[&str] = &[
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

fn truncate_result(content: &mut String) {
    if let Some((cut, _)) = content.char_indices().nth(MAX_RESULT_CHARS) {
        content.truncate(cut);
        content.push_str("\n[output truncated]");
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolOutcome> {
    args[key]
        .as_str()
        .ok_or_else(|| ToolOutcome::error(format!("missing required string argument '{key}'")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

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
    fn repeated_scans_share_specs_and_tool_token_estimates() {
        let root = temp_path("catalog-shared-specs");
        let config = registry_config(root.join("home"), root.join("project"));
        let mut cache = CatalogCache::default();

        let first = Registry::scan(&config, &mut cache);
        let second = Registry::scan(&config, &mut cache);

        let first_specs = first.specs();
        let second_specs = second.specs();
        assert!(!first_specs.is_empty());
        for (left, right) in first_specs.iter().zip(&second_specs) {
            assert!(
                Arc::ptr_eq(left, right),
                "unchanged scans must share tool schemas"
            );
        }
        assert!(first.tool_tokens() > 0);
        assert_eq!(first.tool_tokens(), second.tool_tokens());
    }

    #[test]
    fn web_tools_are_advertised_only_when_enabled() {
        let mut config = registry_config(temp_path("web-home"), temp_path("web-project"));
        let mut cache = CatalogCache::default();
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
            .map(|spec| spec.name.clone())
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
        let mut cache = CatalogCache::default();
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
    fn background_tools_are_main_agent_only() {
        let root = temp_path("background-registry");
        let config = registry_config(root.join("home"), root.join("project"));
        let mut cache = CatalogCache::default();
        let child = Registry::scan(&config, &mut cache);
        let child_names = child
            .specs()
            .into_iter()
            .map(|spec| spec.name.clone())
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
            .map(|spec| spec.name.clone())
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
            Registry::scan_with_background(&config, &mut CatalogCache::default(), manager.clone());
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
        let mut cache = CatalogCache::default();
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
        let mut cache = CatalogCache::default();

        let disabled = Registry::scan(&config, &mut cache);
        assert!(!disabled.has_subagent_tools());
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
        assert!(enabled.has_subagent_tools());
        let names = enabled
            .specs()
            .into_iter()
            .map(|spec| spec.name.clone())
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
    fn spawn_without_model_inherits_the_active_parent() {
        let root = temp_path("spawn-model-inherit");
        let mut config = registry_config(root.join("home"), root.join("project"));
        config.openai_base_url = "http://127.0.0.1:9/v1".into();
        let manager = SubagentManager::new("session".into(), config.max_subagents);
        let registry = Registry::scan_with_subagents(
            &config,
            &mut CatalogCache::default(),
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
    fn scout_rejects_spawn_tasks_that_require_file_writes() {
        let root = temp_path("scout-write-capability");
        let config = registry_config(root.join("home"), root.join("project"));
        let manager = SubagentManager::new("session".into(), config.max_subagents);
        let registry = Registry::scan_with_subagents(
            &config,
            &mut CatalogCache::default(),
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
        let mut registry = Registry::scan(&config, &mut CatalogCache::default());

        registry.retain_names(&["read_file".to_string(), "shell".to_string()]);

        let mut names = registry
            .specs()
            .into_iter()
            .map(|spec| spec.name.clone())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, ["read_file", "shell"]);
    }

    #[test]
    fn main_agent_does_not_advertise_scout_discovery_tools() {
        let root = temp_path("main-no-scout-tools");
        let config = registry_config(root.join("home"), root.join("project"));
        let manager = SubagentManager::new("main".into(), 1);
        let registries = [
            Registry::scan(&config, &mut CatalogCache::default()),
            Registry::scan_for_child(&config, &mut CatalogCache::default(), None),
            Registry::scan_for_main_listing(&config, &mut CatalogCache::default()),
            Registry::scan_with_subagents(
                &config,
                &mut CatalogCache::default(),
                manager.clone(),
                "test",
            ),
        ];
        for registry in registries {
            let names = registry
                .specs()
                .into_iter()
                .map(|spec| spec.name.clone())
                .collect::<Vec<_>>();
            assert!(names.iter().any(|name| name == "shell"));
            for name in ["list_files", "search_files", "git_inspect"] {
                assert!(
                    !names.iter().any(|candidate| candidate == name),
                    "main advertised {name}"
                );
                assert!(registry.execute(name, "{}", "main").is_error);
            }
        }
        manager.shutdown_and_discard();
    }

    #[test]
    fn scout_discovers_code_with_native_tools_and_cannot_run_commands() {
        let root = temp_path("scout-native-discovery");
        let tools_dir = root.join("home/tools");
        std::fs::create_dir_all(&tools_dir).expect("tools directory");
        std::fs::create_dir_all(root.join("project/src")).expect("source directory");
        std::fs::write(
            root.join("project/src/example.rs"),
            "fn cancellation_entry() {}\n",
        )
        .expect("source");
        for name in ["list_files", "search_files", "git_inspect"] {
            let path = tools_dir.join(name);
            let spec = json!({"name": name, "description": "override", "input_schema": {"type": "object"}});
            std::fs::write(&path, format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", spec))
                .expect("exec fixture");
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .expect("executable");
        }
        let config = registry_config(root.join("home"), root.join("project"));
        let scout = crate::subagent::presets::bundled().remove(0);
        let mut child = crate::agent::Conversation::memory(config, "test".into(), "scout".into());
        child.set_tool_allowlist(scout.tools.expect("scout allowlist"));
        let registry = child.scan_tools();
        assert!(
            registry
                .warnings
                .iter()
                .filter(|warning| warning.contains("reserved"))
                .count()
                >= 3
        );
        assert!(
            registry
                .specs()
                .iter()
                .any(|spec| spec.name == "git_inspect")
        );
        assert!(
            registry
                .execute("git_inspect", r#"{"operation":"reset"}"#, "scout")
                .is_error
        );
        let listing = registry.execute(
            "list_files",
            &json!({"path": root.join("project")}).to_string(),
            "scout",
        );
        assert!(!listing.is_error, "{}", listing.content);
        assert!(listing.content.contains("example.rs"));
        let search = registry.execute(
            "search_files",
            &json!({"path": root.join("project"), "query": "cancellation_entry"}).to_string(),
            "scout",
        );
        assert!(!search.is_error, "{}", search.content);
        assert!(
            search
                .content
                .contains("example.rs:1:4: fn cancellation_entry() {}")
        );
        assert!(
            registry
                .execute("shell", r#"{"command":"true"}"#, "scout")
                .is_error
        );
        assert!(registry.execute("write_file", "{}", "scout").is_error);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn planning_registry_gates_shell_to_read_only_inspection() {
        let root = temp_path("planning-registry");
        let mut config = registry_config(root.join("home"), root.join("project"));
        config.web_browsing = true;
        let manager = SubagentManager::new("session".into(), config.max_subagents);
        let mut registry = Registry::scan_with_subagents_and_background(
            &config,
            &mut CatalogCache::default(),
            manager.clone(),
            "test",
            BackgroundProcessManager::default(),
        );
        let broker = QuestionBroker::default();
        broker.enable();
        registry.advertise_user_input(broker);
        assert!(registry.has_subagent_tools());

        registry.retain_for_planning();
        registry.advertise_plan_complete();
        assert!(!registry.has_subagent_tools());

        let names = registry
            .specs()
            .into_iter()
            .map(|spec| spec.name.clone())
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

        let mut registry = Registry::scan(&config, &mut CatalogCache::default());
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
