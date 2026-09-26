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
         writes, commands, or unsupported tools. scout is read-only. Set model only when the \
         user explicitly requests a listed model; it overrides preset and default models. \
         Agents: {available_agents}."
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
                    "model": {
                        "type": "string",
                        "description": "Optional provider:model ID the user explicitly requested."
                    },
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
            "Steer child; queue=true adds a turn; restart if settled.",
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "message": {"type": "string"},
                    "queue": {"type": "boolean"}
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
            let model = match args.get("model") {
                Some(value) => value
                    .as_str()
                    .map(Some)
                    .ok_or_else(|| "'model' must be a string when provided".to_string()),
                None => Ok(None),
            };
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
                        let model = model?;
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
                            .spawn_with_model(
                                context.config.clone(),
                                &context.parent_model,
                                supplied_name,
                                prompt,
                                preset,
                                model,
                            )
                            .map(|id| format!("started {id}"))
                    })
                })
            })
        }
        SubagentTool::Send => {
            let id = str_arg(args, "id").map_err(|error| error.content);
            let message = str_arg(args, "message").map_err(|error| error.content);
            let queue = match args.get("queue") {
                Some(value) => value
                    .as_bool()
                    .ok_or_else(|| "'queue' must be a boolean".to_string()),
                None => Ok(false),
            };
            id.and_then(|id| {
                message.and_then(|message| {
                    queue.and_then(|queue| {
                        if queue {
                            context.manager.send(id, message, RunOrigin::Model)
                        } else {
                            context
                                .manager
                                .steer_with_origin(id, message, RunOrigin::Model)
                        }
                    })
                })
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
            .iter()
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
        assert!(
            spawn
                .description
                .contains("Set model only when the user explicitly requests a listed model")
        );
        let wait = specs
            .iter()
            .find(|spec| spec.name == "subagent_wait")
            .expect("wait tool present");
        assert!(wait.description.contains("Omit timeout_secs to block"));
        let cancel = specs
            .iter()
            .find(|spec| spec.name == "subagent_cancel")
            .expect("cancel tool present");
        assert!(
            cancel
                .description
                .contains("Use only when the work is no longer needed")
        );
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
            properties["model"].get("enum").is_none(),
            "the model catalog must not churn the tool schema"
        );
        assert!(
            properties["agent"]["description"]
                .as_str()
                .is_some_and(|text| text.contains("Scout is read-only"))
        );
    }

    #[test]
    fn send_tool_advertises_optional_queue_mode() {
        let send = entries(&[])
            .into_iter()
            .find(|entry| entry.spec.name == "subagent_send")
            .expect("send tool");
        assert!(send.spec.description.contains("Steer child"));
        assert_eq!(
            send.spec.input_schema["properties"]["queue"]["type"],
            "boolean"
        );
        assert_eq!(send.spec.input_schema["required"], json!(["id", "message"]));
    }

    #[test]
    fn send_steers_a_running_child_unless_queue_is_requested() {
        use std::net::TcpListener;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let mut config = crate::config::Config::test_default();
        config.providers.insert(
            "local".into(),
            crate::config::ProviderConfig {
                base_url: format!("http://{}/v1", listener.local_addr().expect("address")),
                api: "openai-completions".into(),
                api_key: None,
                auth_header: Some(false),
                headers: Default::default(),
                models: Vec::new(),
                compat: Default::default(),
            },
        );
        let manager = SubagentManager::new("session".into(), 1);
        let id = manager
            .spawn(config.clone(), "local:model", None, "initial task", None)
            .expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(5);
        let connection = loop {
            match listener.accept() {
                Ok((connection, _)) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "child did not connect");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("provider connection: {error}"),
            }
        };
        let context = SubagentContext {
            manager: manager.clone(),
            config,
            parent_model: "local:model".into(),
            presets: Vec::new(),
        };
        let default = execute(
            Some(&context),
            SubagentTool::Send,
            &json!({"id": id.as_str(), "message": "finish with a summary"}),
        );
        let queued = execute(
            Some(&context),
            SubagentTool::Send,
            &json!({"id": id.as_str(), "message": "another turn", "queue": true}),
        );
        let snapshot = manager.snapshots().pop().expect("child snapshot");
        assert!(!default.is_error, "{}", default.content);
        assert_eq!(default.content, format!("steering {id}"));
        assert!(!queued.is_error, "{}", queued.content);
        assert_eq!(queued.content, format!("queued message for {id}"));
        assert_eq!(snapshot.pending_steers, ["finish with a summary"]);
        assert_eq!(snapshot.queued_messages.len(), 1);
        assert_eq!(snapshot.queued_messages[0].text, "another turn");
        let invalid = execute(
            Some(&context),
            SubagentTool::Send,
            &json!({"id": id.as_str(), "message": "ignored", "queue": "yes"}),
        );
        assert!(invalid.is_error);
        assert!(invalid.content.contains("'queue' must be a boolean"));
        drop(connection);
        drop(listener);
        manager.shutdown_and_discard();
    }

    #[test]
    fn sending_to_a_settled_child_restarts_a_model_originated_run() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let mut config = crate::config::Config::test_default();
        config.providers.insert(
            "local".into(),
            crate::config::ProviderConfig {
                base_url: format!("http://{}/v1", listener.local_addr().expect("address")),
                api: "openai-completions".into(),
                api_key: None,
                auth_header: Some(false),
                headers: Default::default(),
                models: Vec::new(),
                compat: Default::default(),
            },
        );
        let server = std::thread::spawn(move || {
            for answer in ["first", "follow-up"] {
                let (mut stream, _) = listener.accept().expect("provider connection");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("read timeout");
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let read = stream.read(&mut buffer).expect("provider request");
                    assert!(read > 0, "request closed before body");
                    request.extend_from_slice(&buffer[..read]);
                    if let Some(header_end) =
                        request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                    {
                        let headers = String::from_utf8_lossy(&request[..header_end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        if request.len() >= header_end + 4 + length {
                            break;
                        }
                    }
                }
                let body = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{answer}\"}},\"finish_reason\":null}}]}}\n\ndata: [DONE]\n\n"
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("provider response");
                stream.flush().expect("flush provider response");
            }
        });
        let manager = SubagentManager::new("session".into(), 1);
        let id = manager
            .spawn(config.clone(), "local:model", None, "initial task", None)
            .expect("spawn");
        assert!(manager.wait_all(5), "initial run should settle");
        manager.drain_deferred();
        let context = SubagentContext {
            manager: manager.clone(),
            config,
            parent_model: "local:model".into(),
            presets: Vec::new(),
        };
        let sent = execute(
            Some(&context),
            SubagentTool::Send,
            &json!({"id": id.as_str(), "message": "finish the follow-up"}),
        );
        assert!(!sent.is_error, "{}", sent.content);
        assert_eq!(sent.content, format!("restarted {id}"));
        assert!(manager.wait_all(5), "follow-up should settle");
        let deliveries = manager.drain_deferred();
        assert_eq!(deliveries.len(), 1, "model follow-up must be delivered");
        assert!(
            deliveries[0].result.contains("follow-up"),
            "unexpected model follow-up delivery: {:?}; error: {:?}; snapshots: {:?}",
            deliveries[0].result,
            deliveries[0].error,
            manager.snapshots()
        );
        server.join().expect("server");
        manager.shutdown_and_discard();
    }

    #[test]
    fn spawn_rejects_unlisted_model_overrides() {
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
            outcome.content.contains("not in the available model list")
                && outcome.content.contains("available: none"),
            "{}",
            outcome.content
        );
        let non_string = execute(
            Some(&context),
            SubagentTool::Spawn,
            &serde_json::json!({
                "prompt": "Inspect the requested file.",
                "required_tools": ["read_file"],
                "model": 7
            }),
        );
        assert!(non_string.content.contains("'model' must be a string"));
        context.manager.shutdown_and_discard();
    }
}
