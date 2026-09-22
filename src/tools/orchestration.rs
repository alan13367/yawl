//! Subagent orchestration tools: spawn, message, wait for, and cancel child
//! agents. The manager, capacity, and delivery live in `crate::subagent`.

use serde_json::{Value, json};

use super::catalog::CatalogCache;
use super::{ToolEntry, ToolImpl, ToolOutcome, str_arg};
use crate::config::Config;
use crate::provider::ToolSpec;
use crate::subagent::{AgentPreset, RunOrigin, SubagentManager};

#[derive(Clone, Copy)]
pub(super) enum SubagentTool {
    Spawn,
    Send,
    Wait,
    Cancel,
    List,
}

pub(super) struct SubagentContext {
    manager: SubagentManager,
    config: Config,
    parent_model: String,
    presets: Vec<AgentPreset>,
}

impl super::Registry {
    pub(crate) fn enable_subagents(
        &mut self,
        config: &Config,
        cache: &mut CatalogCache,
        manager: SubagentManager,
        parent_model: &str,
    ) {
        let (entries, presets, warnings) = cache.subagent_entries(config);
        self.warnings.extend(warnings);
        self.entries.extend(entries);
        self.subagents = Some(SubagentContext {
            manager,
            config: config.clone(),
            parent_model: parent_model.to_string(),
            presets,
        });
    }
}

pub(super) fn entries(presets: &[AgentPreset]) -> Vec<ToolEntry> {
    let tool = |name: &str, description: &str, input_schema: Value, imp| {
        ToolEntry::new(
            ToolSpec {
                name: name.into(),
                description: description.into(),
                input_schema,
            },
            ToolImpl::Subagent(imp),
        )
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
             bounded status check. Long results include an excerpt and a full report path.",
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
            "List all subagents, or status and result for one ID; long results link to full reports.",
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

pub(super) fn execute(
    context: Option<&SubagentContext>,
    tool: SubagentTool,
    args: &Value,
) -> ToolOutcome {
    let Some(context) = context else {
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::presets::bundled;

    #[test]
    fn spawn_tool_lists_presets_and_takes_an_optional_agent() {
        let presets = bundled();
        let specs = entries(&presets)
            .into_iter()
            .map(|entry| entry.spec)
            .collect::<Vec<_>>();
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
            spawn
                .description
                .contains("scout (read_file+read_skill+list_files+search_files+git_inspect)"),
            "the description should advertise bundled presets; got:\n{}",
            spawn.description
        );
        assert!(spawn.description.contains("# Target"));
        let required = spawn.input_schema.get("required").expect("required list");
        assert_eq!(
            required,
            &serde_json::json!(["prompt", "required_tools"]),
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
    fn spawn_rejects_model_overrides_from_the_orchestrator() {
        let presets = bundled();
        let context = SubagentContext {
            manager: crate::subagent::SubagentManager::new("session".into(), 1),
            config: crate::config::Config::test_default(),
            parent_model: "openai-codex:parent".into(),
            presets,
        };
        let args = serde_json::json!({
            "prompt": "Inspect the requested file.",
            "required_tools": ["read_file"],
            "model": "gpt-4.1-mini"
        });
        let outcome = execute(Some(&context), SubagentTool::Spawn, &args);
        assert!(outcome.is_error, "unexpected outcome: {}", outcome.content);
        assert!(
            outcome.content.contains("not accepted"),
            "{}",
            outcome.content
        );
        context.manager.shutdown_and_discard();
    }
}
