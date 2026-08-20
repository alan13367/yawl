//! The agent loop: send messages → stream → execute tool calls → append
//! results → repeat until the model stops calling tools. Iterations are
//! uncapped; Ctrl+C aborts the in-flight turn, not the process.

use crate::compaction;
use crate::config::{Config, ConfigChange, ConfigChangeEffect};
use crate::error::Error;
use crate::provider::{self, Message, ReasoningKind, StreamNotice, ToolCall, stream_turn};
use crate::session::Session;
use crate::tools::{DescribeCache, Registry};

/// Progress events surfaced to the UI (print mode or TUI) during a turn.
pub enum TurnEvent<'a> {
    TextDelta(&'a str),
    ReasoningDelta {
        kind: ReasoningKind,
        text: &'a str,
    },
    /// Discard any partial text shown so far; a retry restarts the response.
    RetryReset,
    Retrying {
        attempt: u32,
        delay_ms: u64,
        error: String,
    },
    /// One assistant response finished (there may be more after tools run).
    AssistantDone,
    ToolStart {
        name: &'a str,
        args: &'a str,
    },
    ToolEnd {
        name: &'a str,
        output: &'a str,
        is_error: bool,
    },
    Compacting,
    Compacted {
        replaced: usize,
    },
    Usage {
        context_tokens: u64,
        context_window: u64,
    },
}

pub struct Agent {
    config: Config,
    /// Current model spec (may carry an `anthropic:`/`openai:` prefix);
    /// switchable mid-session via `/model`.
    model: String,
    messages: Vec<Message>,
    session: Session,
    /// Last provider-reported total (input + output) tokens — the best
    /// estimate of current context usage.
    context_tokens: u64,
    describe_cache: DescribeCache,
}

impl Agent {
    pub fn new(config: Config, model: String, session: Session, messages: Vec<Message>) -> Agent {
        Agent {
            config,
            model,
            messages,
            session,
            context_tokens: 0,
            describe_cache: DescribeCache::default(),
        }
    }

    pub fn context_window(&self) -> u64 {
        crate::model::context_window(&self.config, &self.model)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn session_id(&self) -> &str {
        &self.session.id
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn context_tokens(&self) -> u64 {
        self.context_tokens
    }

    pub(crate) fn switch_model(&mut self, model: String) {
        self.model = model;
    }

    pub(crate) fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.config.reasoning_effort = effort;
    }

    pub(crate) fn sync_display_config(&mut self, config: &Config) {
        self.config.hide_reasoning = config.hide_reasoning;
        self.config.accent_color = config.accent_color;
    }

    /// Starts a fresh session (used by `/new` and `/clear`).
    pub fn reset(&mut self) -> Result<(), Error> {
        self.session = Session::create(&self.config.sessions_dir())?;
        self.messages.clear();
        self.context_tokens = 0;
        Ok(())
    }

    /// Replaces the conversation with a saved session (used by `/resume`).
    pub fn load_session(&mut self, id: &str) -> Result<(), Error> {
        let (session, messages) = Session::open(&self.config.sessions_dir(), id)?;
        self.session = session;
        self.messages = messages;
        self.context_tokens = 0;
        Ok(())
    }

    pub fn scan_tools(&mut self) -> Registry {
        Registry::scan(&self.config, &mut self.describe_cache)
    }

    pub(crate) fn change_global_config(
        &mut self,
        change: ConfigChange,
    ) -> Result<ConfigChangeEffect, Error> {
        let changes_model = matches!(&change, ConfigChange::Model(_));
        let outcome = self.config.change_global(change)?;
        self.config = outcome.config;
        if changes_model {
            self.model = self
                .config
                .model
                .clone()
                .ok_or_else(|| Error::Config("no model configured".into()))?;
            self.context_tokens = 0;
        }
        Ok(outcome.effect)
    }

    /// Runs one full turn. `user_input` is `None` when re-driving an existing
    /// conversation (not used by the current front ends, but harmless).
    ///
    /// Returns `Ok(true)` if the turn completed, `Ok(false)` if it was
    /// aborted by Ctrl+C.
    pub fn run_turn(
        &mut self,
        user_input: Option<String>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        self.run_turn_with(user_input, sink, &mut provider::resolve)
    }

    fn run_turn_with<F>(
        &mut self,
        user_input: Option<String>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<bool, Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        crate::set_interrupted(false);
        if let Some(input) = user_input {
            let msg = Message::user(input);
            self.session.append_message(&msg)?;
            self.messages.push(msg);
        }
        let system = crate::prompt::build_system_prompt(&self.config.home_dir);

        // Uncapped: the loop ends when the model stops calling tools.
        loop {
            if crate::interrupted() {
                return Ok(false);
            }
            // Rescan every iteration so a tool the model just wrote is
            // available on its very next step.
            let registry = self.scan_tools();
            let specs = registry.specs();

            self.maybe_compact(sink)?;

            let (provider, bare_model) = resolve_provider(&self.model, &self.config)?;
            let request = provider::Request {
                model: &bare_model,
                system: &system,
                messages: &self.messages,
                tools: &specs,
                max_tokens: crate::model::max_tokens(&self.config, &self.model),
            };
            let out = match stream_turn(provider.as_ref(), &request, &mut forward(sink)) {
                Ok(out) => out,
                // Abort quietly: partial output is discarded, history stays
                // valid (it still ends with a user/tool message).
                Err(Error::Interrupted) => return Ok(false),
                Err(e) => return Err(e),
            };

            self.context_tokens = out.input_tokens + out.output_tokens;
            sink(TurnEvent::Usage {
                context_tokens: self.context_tokens,
                context_window: self.context_window(),
            });

            let mut assistant = Message::assistant(out.text, out.tool_calls.clone());
            assistant.reasoning = out.reasoning;
            assistant.provider_data = out.provider_data;
            self.session.append_message(&assistant)?;
            self.messages.push(assistant);
            sink(TurnEvent::AssistantDone);

            if out.tool_calls.is_empty() {
                return Ok(true);
            }
            let aborted = self.run_tools(&registry, &out.tool_calls, sink)?;
            if aborted {
                return Ok(false);
            }
        }
    }

    /// Executes tool calls in order. On interrupt, remaining calls get
    /// synthetic error results so every tool call keeps a paired result and
    /// the history stays valid for the next request.
    fn run_tools(
        &mut self,
        registry: &Registry,
        calls: &[ToolCall],
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        self.run_tools_while(registry, calls, sink, crate::interrupted)
    }

    fn run_tools_while(
        &mut self,
        registry: &Registry,
        calls: &[ToolCall],
        sink: &mut dyn FnMut(TurnEvent<'_>),
        mut interrupted: impl FnMut() -> bool,
    ) -> Result<bool, Error> {
        let mut aborted = false;
        for call in calls {
            let result = if aborted || interrupted() {
                aborted = true;
                Message::tool_result(
                    &call.id,
                    &call.name,
                    "[interrupted by user]".to_string(),
                    true,
                )
            } else {
                sink(TurnEvent::ToolStart {
                    name: &call.name,
                    args: &call.arguments,
                });
                let outcome = registry.execute(&call.name, &call.arguments, &self.session.id);
                sink(TurnEvent::ToolEnd {
                    name: &call.name,
                    output: &outcome.content,
                    is_error: outcome.is_error,
                });
                if interrupted() {
                    aborted = true;
                }
                Message::tool_result(&call.id, &call.name, outcome.content, outcome.is_error)
            };
            self.session.append_message(&result)?;
            self.messages.push(result);
        }
        Ok(aborted)
    }

    fn maybe_compact(&mut self, sink: &mut dyn FnMut(TurnEvent<'_>)) -> Result<(), Error> {
        if !self.config.auto_compact
            || !compaction::should_compact(
                self.context_tokens,
                self.context_window(),
                self.config.compact_threshold,
            )
        {
            return Ok(());
        }
        match self.compact_now(sink) {
            Ok(()) => Ok(()),
            // A failed auto-compaction shouldn't kill the turn; the request
            // may still fit. Manual /compact reports errors directly.
            Err(Error::Interrupted) => Err(Error::Interrupted),
            Err(_) => Ok(()),
        }
    }

    /// Summarizes the head of the conversation with the current model
    /// (also the `/compact` slash command).
    pub fn compact_now(&mut self, sink: &mut dyn FnMut(TurnEvent<'_>)) -> Result<(), Error> {
        sink(TurnEvent::Compacting);
        let (provider, bare_model) = provider::resolve(&self.model, &self.config)?;
        let (summary, replaced) = compaction::compact(
            provider.as_ref(),
            &bare_model,
            crate::model::max_tokens(&self.config, &self.model),
            &mut self.messages,
            // Summarizer output is not user-facing; swallow its deltas.
            &mut |_| {},
        )?;
        self.session.append_compaction(&summary, replaced)?;
        // Old usage estimate is stale after compaction; a fresh number
        // arrives with the next response.
        self.context_tokens = 0;
        sink(TurnEvent::Compacted { replaced });
        Ok(())
    }
}

/// Adapts stream-level notices to turn events.
fn forward<'s>(sink: &'s mut dyn FnMut(TurnEvent<'_>)) -> impl FnMut(StreamNotice<'_>) + 's {
    move |notice| match notice {
        StreamNotice::TextDelta(t) => sink(TurnEvent::TextDelta(t)),
        StreamNotice::ReasoningDelta { kind, text } => {
            sink(TurnEvent::ReasoningDelta { kind, text })
        }
        StreamNotice::RetryReset => sink(TurnEvent::RetryReset),
        StreamNotice::Retrying {
            attempt,
            delay_ms,
            error,
        } => sink(TurnEvent::Retrying {
            attempt,
            delay_ms,
            error,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::config::{DEFAULT_MAX_TOKENS, ProviderConfig};
    use crate::provider::{Event as ProviderEvent, Provider, Request, Role};

    enum ProviderStep {
        Output {
            text: &'static str,
            tool_calls: Vec<ToolCall>,
        },
        Fail,
    }

    struct ScriptedProvider {
        steps: Rc<RefCell<VecDeque<ProviderStep>>>,
        requests: Rc<RefCell<Vec<Vec<Role>>>>,
    }

    impl Provider for ScriptedProvider {
        fn stream_once(
            &self,
            request: &Request<'_>,
            on_event: &mut dyn FnMut(ProviderEvent),
        ) -> Result<(), Error> {
            self.requests.borrow_mut().push(
                request
                    .messages
                    .iter()
                    .map(|message| message.role)
                    .collect(),
            );
            let Some(step) = self.steps.borrow_mut().pop_front() else {
                return Err(Error::Protocol("test provider script exhausted".into()));
            };
            match step {
                ProviderStep::Output { text, tool_calls } => {
                    if !text.is_empty() {
                        on_event(ProviderEvent::TextDelta(text.into()));
                    }
                    for call in tool_calls {
                        on_event(ProviderEvent::ToolCall(call));
                    }
                    on_event(ProviderEvent::Usage {
                        input_tokens: 10,
                        output_tokens: 2,
                    });
                    on_event(ProviderEvent::Done);
                    Ok(())
                }
                ProviderStep::Fail => Err(Error::Protocol("scripted failure".into())),
            }
        }
    }

    struct TestAgent {
        root: PathBuf,
        agent: Agent,
    }

    impl TestAgent {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("yawl-agent-{}-{nonce}-{name}", std::process::id()));
            let home_dir = root.join("home");
            let project_dir = root.join("project");
            let config = Config {
                model: Some("test".into()),
                anthropic_base_url: String::new(),
                openai_base_url: String::new(),
                max_tokens: DEFAULT_MAX_TOKENS,
                reasoning_effort: None,
                hide_reasoning: false,
                accent_color: crate::config::UiColor::WHITE,
                context_windows: HashMap::new(),
                auto_compact: false,
                compact_threshold: 0.85,
                skill_dirs: Vec::new(),
                providers: HashMap::<String, ProviderConfig>::new(),
                home_dir: home_dir.clone(),
                project_dir,
            };
            let session =
                Session::create(&config.sessions_dir()).expect("test session should be created");
            Self {
                root,
                agent: Agent::new(config, "test".into(), session, Vec::new()),
            }
        }
    }

    impl Drop for TestAgent {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn conversation_transaction_persists_tool_loop_in_order() {
        let mut test = TestAgent::new("tool-loop");
        let steps = Rc::new(RefCell::new(VecDeque::from([
            ProviderStep::Output {
                text: "",
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "shell".into(),
                    arguments: r#"{"command":"printf tool-output"}"#.into(),
                }],
            },
            ProviderStep::Output {
                text: "done",
                tool_calls: Vec::new(),
            },
        ])));
        let requests = Rc::new(RefCell::new(Vec::new()));
        let mut resolve = |_: &str, _: &Config| {
            Ok::<(Box<dyn Provider>, String), Error>((
                Box::new(ScriptedProvider {
                    steps: Rc::clone(&steps),
                    requests: Rc::clone(&requests),
                }),
                "test".into(),
            ))
        };

        let completed = test
            .agent
            .run_turn_with(Some("run it".into()), &mut |_| {}, &mut resolve)
            .expect("scripted turn should complete");

        assert!(completed);
        assert_eq!(
            test.agent
                .messages
                .iter()
                .map(|message| message.role)
                .collect::<Vec<_>>(),
            [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
        );
        assert_eq!(test.agent.messages[2].content, "tool-output");
        assert_eq!(test.agent.messages[3].content, "done");
        assert_eq!(
            requests.borrow().as_slice(),
            [
                vec![Role::User],
                vec![Role::User, Role::Assistant, Role::Tool]
            ]
        );
        let (_, replayed) =
            Session::open(&test.agent.config.sessions_dir(), &test.agent.session.id)
                .expect("persisted session should replay");
        assert_eq!(
            replayed
                .iter()
                .map(|message| message.role)
                .collect::<Vec<_>>(),
            [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
        );
    }

    #[test]
    fn failed_provider_does_not_persist_partial_assistant() {
        let mut test = TestAgent::new("provider-failure");
        let steps = Rc::new(RefCell::new(VecDeque::from([ProviderStep::Fail])));
        let requests = Rc::new(RefCell::new(Vec::new()));
        let mut resolve = |_: &str, _: &Config| {
            Ok::<(Box<dyn Provider>, String), Error>((
                Box::new(ScriptedProvider {
                    steps: Rc::clone(&steps),
                    requests: Rc::clone(&requests),
                }),
                "test".into(),
            ))
        };

        let result = test
            .agent
            .run_turn_with(Some("hello".into()), &mut |_| {}, &mut resolve);

        assert!(result.is_err());
        assert_eq!(test.agent.messages.len(), 1);
        assert_eq!(test.agent.messages[0].role, Role::User);
    }

    #[test]
    fn interrupted_tool_batch_keeps_one_result_per_call() {
        let mut test = TestAgent::new("interrupted-tools");
        let registry = test.agent.scan_tools();
        let calls = [
            ToolCall {
                id: "one".into(),
                name: "shell".into(),
                arguments: r#"{"command":"true"}"#.into(),
            },
            ToolCall {
                id: "two".into(),
                name: "shell".into(),
                arguments: r#"{"command":"true"}"#.into(),
            },
        ];

        let aborted = test
            .agent
            .run_tools_while(&registry, &calls, &mut |_| {}, || true)
            .expect("synthetic tool results should persist");

        assert!(aborted);
        assert_eq!(test.agent.messages.len(), 2);
        assert!(test.agent.messages.iter().all(|message| {
            message.role == Role::Tool
                && message.is_error
                && message.content == "[interrupted by user]"
        }));
        assert_eq!(test.agent.messages[0].tool_call_id.as_deref(), Some("one"));
        assert_eq!(test.agent.messages[1].tool_call_id.as_deref(), Some("two"));
    }
}
