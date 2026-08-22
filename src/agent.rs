//! The agent loop: send messages → stream → execute tool calls → append
//! results → repeat until the model stops calling tools. Iterations are
//! uncapped; Ctrl+C aborts the in-flight turn, not the process.

use crate::cancellation::CancellationToken;
use crate::compaction;
use crate::config::{Config, ConfigChange, ConfigChangeEffect};
use crate::error::Error;
use crate::provider::{
    self, Message, ReasoningKind, StreamNotice, SubagentResult, ToolCall, stream_turn,
};
use crate::session::Session;
use crate::subagent::SubagentManager;
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
    /// Non-fatal problem the user should know about; the turn continues.
    Warning(String),
    Usage {
        context_tokens: u64,
        context_window: u64,
    },
}

struct Journal {
    id: String,
    persistent: Option<Session>,
}

impl Journal {
    fn persistent(session: Session) -> Self {
        Self {
            id: session.id.clone(),
            persistent: Some(session),
        }
    }

    fn memory(id: String) -> Self {
        Self {
            id,
            persistent: None,
        }
    }

    fn append_message(&mut self, message: &Message) -> Result<(), Error> {
        match self.persistent.as_mut() {
            Some(session) => session.append_message(message),
            None => Ok(()),
        }
    }

    fn append_compaction(&mut self, summary: &str, replaced: usize) -> Result<(), Error> {
        match self.persistent.as_mut() {
            Some(session) => session.append_compaction(summary, replaced),
            None => Ok(()),
        }
    }
}

/// Provider-neutral conversation state shared by the main agent and
/// memory-only subagents.
pub(crate) struct Conversation {
    config: Config,
    /// Current model spec (may carry an `anthropic:`/`openai:` prefix);
    /// switchable mid-session via `/model`.
    model: String,
    messages: Vec<Message>,
    session: Journal,
    /// Last provider-reported total (input + output) tokens — the best
    /// estimate of current context usage.
    context_tokens: u64,
    latest_turn_result: String,
    describe_cache: DescribeCache,
    cancellation: CancellationToken,
    subagents: Option<SubagentManager>,
    print_mode: bool,
}

impl Conversation {
    fn persistent(config: Config, model: String, session: Session, messages: Vec<Message>) -> Self {
        let subagents = SubagentManager::new(session.id.clone(), config.max_subagents);
        Self {
            config,
            model,
            messages,
            session: Journal::persistent(session),
            context_tokens: 0,
            latest_turn_result: String::new(),
            describe_cache: DescribeCache::default(),
            cancellation: CancellationToken::default(),
            subagents: Some(subagents),
            print_mode: false,
        }
    }

    pub(crate) fn memory(config: Config, model: String, session_id: String) -> Self {
        Self {
            config,
            model,
            messages: Vec::new(),
            session: Journal::memory(session_id),
            context_tokens: 0,
            latest_turn_result: String::new(),
            describe_cache: DescribeCache::default(),
            cancellation: CancellationToken::default(),
            subagents: None,
            print_mode: false,
        }
    }

    pub(crate) fn context_window(&self) -> u64 {
        crate::model::context_window(&self.config, &self.model)
    }

    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session.id
    }

    pub(crate) fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub(crate) fn context_tokens(&self) -> u64 {
        self.context_tokens
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(crate) fn subagent_manager(&self) -> Option<SubagentManager> {
        self.subagents.clone()
    }

    pub(crate) fn latest_turn_result(&self) -> String {
        self.latest_turn_result.clone()
    }

    fn append_input_message(&mut self, message: Message) -> Result<(), Error> {
        self.session.append_message(&message)?;
        self.messages.push(message);
        Ok(())
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
        self.config.scroll_bar = config.scroll_bar;
    }

    /// Starts a fresh session (used by `/new` and `/clear`).
    pub fn reset(&mut self) -> Result<(), Error> {
        let session = Session::create(&self.config.sessions_dir())?;
        if let Some(manager) = &self.subagents {
            manager.shutdown_and_discard();
        }
        self.subagents = Some(SubagentManager::new(
            session.id.clone(),
            self.config.max_subagents,
        ));
        self.session = Journal::persistent(session);
        self.messages.clear();
        self.context_tokens = 0;
        self.latest_turn_result.clear();
        Ok(())
    }

    /// Replaces the conversation with a saved session (used by `/resume`).
    pub fn load_session(&mut self, id: &str) -> Result<(), Error> {
        let (session, messages) = Session::open(&self.config.sessions_dir(), id)?;
        if let Some(manager) = &self.subagents {
            manager.shutdown_and_discard();
        }
        self.subagents = Some(SubagentManager::new(
            session.id.clone(),
            self.config.max_subagents,
        ));
        self.session = Journal::persistent(session);
        self.messages = messages;
        self.context_tokens = 0;
        self.latest_turn_result.clear();
        Ok(())
    }

    pub fn scan_tools(&mut self) -> Registry {
        match (&self.subagents, self.config.subagents) {
            (Some(manager), true) => Registry::scan_with_subagents(
                &self.config,
                &mut self.describe_cache,
                manager.clone(),
                &self.model,
            ),
            _ => Registry::scan(&self.config, &mut self.describe_cache),
        }
    }

    pub(crate) fn change_global_config(
        &mut self,
        change: ConfigChange,
    ) -> Result<ConfigChangeEffect, Error> {
        let changes_model = matches!(&change, ConfigChange::Model(_));
        let outcome = self.config.change_global(change)?;
        self.config = outcome.config;
        if let Some(manager) = &self.subagents {
            manager.set_limit(self.config.max_subagents);
        }
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
        let cancellation = self.cancellation.clone();
        crate::cancellation::scope(&cancellation, || {
            self.cancellation.clear();
            self.run_turn_with(user_input, sink, &mut provider::resolve)
        })
    }

    pub(crate) fn run_turn_preserving_cancellation(
        &mut self,
        user_input: Option<String>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        let cancellation = self.cancellation.clone();
        crate::cancellation::scope(&cancellation, || {
            self.run_turn_with(user_input, sink, &mut provider::resolve)
        })
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
        self.latest_turn_result.clear();
        if let Some(input) = user_input {
            self.append_input_message(Message::user(input))?;
        }
        let system = if self.subagents.is_some() {
            crate::prompt::build_system_prompt(
                &self.config.home_dir,
                self.config.subagents,
                self.print_mode,
            )
        } else {
            crate::prompt::build_subagent_system_prompt(&self.config.home_dir)
        };

        // Uncapped: the loop ends when the model stops calling tools.
        loop {
            if crate::cancellation::interrupted() {
                return Ok(false);
            }
            // Rescan every iteration so a tool the model just wrote is
            // available on its very next step.
            let registry = self.scan_tools();
            let specs = registry.specs();

            self.maybe_compact(sink, resolve_provider)?;

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
            if crate::cancellation::interrupted() {
                return Ok(false);
            }

            self.context_tokens = out.input_tokens.saturating_add(out.output_tokens);
            sink(TurnEvent::Usage {
                context_tokens: self.context_tokens,
                context_window: self.context_window(),
            });

            self.latest_turn_result.clone_from(&out.text);
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
        self.run_tools_while(registry, calls, sink, crate::cancellation::interrupted)
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

    fn maybe_compact<F>(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<(), Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        if !self.config.auto_compact
            || !compaction::should_compact(
                self.context_tokens,
                self.context_window(),
                self.config.compact_threshold,
            )
        {
            return Ok(());
        }
        match self.compact_now_with(sink, resolve_provider) {
            // A failed auto-compaction shouldn't kill the turn; the request
            // may still fit. Manual /compact reports errors directly.
            Err(Error::Interrupted) => Err(Error::Interrupted),
            Ok(()) => Ok(()),
            Err(error) => {
                sink(TurnEvent::Warning(format!(
                    "Auto-compaction failed; continuing without compacting: {error}"
                )));
                Ok(())
            }
        }
    }

    /// Summarizes the head of the conversation with the current model
    /// (also the `/compact` slash command).
    pub fn compact_now(&mut self, sink: &mut dyn FnMut(TurnEvent<'_>)) -> Result<(), Error> {
        let cancellation = self.cancellation.clone();
        crate::cancellation::scope(&cancellation, || {
            self.cancellation.clear();
            self.compact_now_with(sink, &mut provider::resolve)
        })
    }

    pub(crate) fn compact_now_preserving_cancellation(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<(), Error> {
        let cancellation = self.cancellation.clone();
        crate::cancellation::scope(&cancellation, || {
            self.compact_now_with(sink, &mut provider::resolve)
        })
    }

    fn compact_now_with<F>(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<(), Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        sink(TurnEvent::Compacting);
        let (provider, bare_model) = resolve_provider(&self.model, &self.config)?;
        let (summary, replaced) = compaction::summarize(
            provider.as_ref(),
            &bare_model,
            crate::model::max_tokens(&self.config, &self.model),
            &self.messages,
            // Summarizer output is not user-facing; swallow its deltas.
            &mut |_| {},
        )?;
        self.session.append_compaction(&summary, replaced)?;
        compaction::apply_summary(&mut self.messages, &summary, replaced);
        // Old usage estimate is stale after compaction; a fresh number
        // arrives with the next response.
        self.context_tokens = 0;
        sink(TurnEvent::Compacted { replaced });
        Ok(())
    }
}

/// A persistent user-facing conversation.
pub struct Agent {
    conversation: Conversation,
}

impl Agent {
    pub fn new(config: Config, model: String, session: Session, messages: Vec<Message>) -> Self {
        Self {
            conversation: Conversation::persistent(config, model, session, messages),
        }
    }

    pub fn context_window(&self) -> u64 {
        self.conversation.context_window()
    }

    pub fn config(&self) -> &Config {
        self.conversation.config()
    }

    pub fn model(&self) -> &str {
        self.conversation.model()
    }

    pub fn session_id(&self) -> &str {
        self.conversation.session_id()
    }

    pub fn messages(&self) -> &[Message] {
        self.conversation.messages()
    }

    pub fn context_tokens(&self) -> u64 {
        self.conversation.context_tokens()
    }

    pub(crate) fn subagents(&self) -> SubagentManager {
        self.conversation
            .subagent_manager()
            .expect("persistent agents always own a subagent manager")
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.conversation.cancellation_token()
    }

    pub(crate) fn clear_cancellation(&self) {
        self.conversation.cancellation.clear();
    }

    /// Disables automatic subagent follow-ups and adjusts orchestration
    /// guidance for a one-shot print-mode run.
    pub fn set_print_mode(&mut self) {
        self.conversation.print_mode = true;
    }

    pub(crate) fn switch_model(&mut self, model: String) {
        self.conversation.switch_model(model);
    }

    pub(crate) fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.conversation.set_reasoning_effort(effort);
    }

    pub(crate) fn sync_display_config(&mut self, config: &Config) {
        self.conversation.sync_display_config(config);
    }

    pub fn reset(&mut self) -> Result<(), Error> {
        self.conversation.reset()
    }

    pub fn load_session(&mut self, id: &str) -> Result<(), Error> {
        self.conversation.load_session(id)
    }

    pub fn scan_tools(&mut self) -> Registry {
        self.conversation.scan_tools()
    }

    pub(crate) fn change_global_config(
        &mut self,
        change: ConfigChange,
    ) -> Result<ConfigChangeEffect, Error> {
        self.conversation.change_global_config(change)
    }

    pub fn run_turn(
        &mut self,
        user_input: Option<String>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        self.conversation.run_turn(user_input, sink)
    }

    pub(crate) fn run_turn_preserving_cancellation(
        &mut self,
        user_input: Option<String>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        self.conversation
            .run_turn_preserving_cancellation(user_input, sink)
    }

    pub(crate) fn has_deferred_subagent_results(&self) -> bool {
        self.subagents().has_deferred()
    }

    pub(crate) fn run_deferred_subagent_results(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<Option<bool>, Error> {
        let deliveries = self.subagents().drain_deferred();
        if deliveries.is_empty() {
            return Ok(None);
        }
        let backup = deliveries.clone();
        let mut results = Vec::new();
        for delivery in deliveries {
            let status = match delivery.outcome {
                crate::subagent::RunOutcome::Completed => "completed",
                crate::subagent::RunOutcome::Failed => "failed",
                crate::subagent::RunOutcome::Interrupted => "interrupted",
            };
            let content = if delivery.error.is_empty() {
                delivery.result
            } else if delivery.result.is_empty() {
                delivery.error
            } else {
                format!("{}\n\nError: {}", delivery.result, delivery.error)
            };
            results.push(SubagentResult {
                id: delivery.id.to_string(),
                name: delivery.name,
                status: status.into(),
                run_number: delivery.run_number,
                content,
            });
        }
        if let Err(error) = self
            .conversation
            .append_input_message(Message::subagent_results(results))
        {
            self.subagents().restore_deferred(backup);
            return Err(error);
        }
        self.conversation
            .run_turn_preserving_cancellation(None, sink)
            .map(Some)
    }

    pub fn compact_now(&mut self, sink: &mut dyn FnMut(TurnEvent<'_>)) -> Result<(), Error> {
        self.conversation.compact_now(sink)
    }

    pub(crate) fn compact_now_preserving_cancellation(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<(), Error> {
        self.conversation.compact_now_preserving_cancellation(sink)
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.subagents().shutdown_and_discard();
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
            input_tokens: u64,
            output_tokens: u64,
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
                ProviderStep::Output {
                    text,
                    tool_calls,
                    input_tokens,
                    output_tokens,
                } => {
                    if !text.is_empty() {
                        on_event(ProviderEvent::TextDelta(text.into()));
                    }
                    for call in tool_calls {
                        on_event(ProviderEvent::ToolCall(call));
                    }
                    on_event(ProviderEvent::Usage {
                        input_tokens,
                        output_tokens,
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
        agent: Conversation,
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
                scroll_bar: true,
                context_windows: HashMap::new(),
                auto_compact: false,
                compact_threshold: 0.85,
                subagents: false,
                max_subagents: crate::config::DEFAULT_MAX_SUBAGENTS,
                subagent_model: crate::config::DEFAULT_SUBAGENT_MODEL.to_string(),
                skill_dirs: Vec::new(),
                providers: HashMap::<String, ProviderConfig>::new(),
                home_dir: home_dir.clone(),
                project_dir,
            };
            let session =
                Session::create(&config.sessions_dir()).expect("test session should be created");
            Self {
                root,
                agent: Conversation::persistent(config, "test".into(), session, Vec::new()),
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
                input_tokens: 10,
                output_tokens: 2,
            },
            ProviderStep::Output {
                text: "done",
                tool_calls: Vec::new(),
                input_tokens: 10,
                output_tokens: 2,
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
    fn provider_usage_saturates_instead_of_wrapping() {
        let mut test = TestAgent::new("saturating-usage");
        let steps = Rc::new(RefCell::new(VecDeque::from([ProviderStep::Output {
            text: "done",
            tool_calls: Vec::new(),
            input_tokens: u64::MAX,
            output_tokens: 1,
        }])));
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
            .run_turn_with(Some("hello".into()), &mut |_| {}, &mut resolve)
            .expect("scripted turn should complete");

        assert!(completed);
        assert_eq!(test.agent.context_tokens(), u64::MAX);
    }

    #[test]
    fn failed_auto_compaction_warns_and_the_turn_continues() {
        let mut test = TestAgent::new("compact-warning");
        test.agent.config.auto_compact = true;
        test.agent.config.context_windows.insert("test".into(), 10);
        // Enough history that auto-compaction has something to summarize.
        for index in 0..12 {
            test.agent
                .messages
                .push(Message::user(format!("history {index}")));
        }
        let steps = Rc::new(RefCell::new(VecDeque::from([
            ProviderStep::Output {
                text: "",
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "shell".into(),
                    arguments: r#"{"command":"true"}"#.into(),
                }],
                input_tokens: 100,
                output_tokens: 0,
            },
            ProviderStep::Fail,
            ProviderStep::Output {
                text: "done",
                tool_calls: Vec::new(),
                input_tokens: 100,
                output_tokens: 0,
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
        let mut warnings = Vec::new();

        let completed = test
            .agent
            .run_turn_with(
                Some("hello".into()),
                &mut |event| {
                    if let TurnEvent::Warning(text) = event {
                        warnings.push(text.to_string());
                    }
                },
                &mut resolve,
            )
            .expect("turn should survive the compaction failure");

        assert!(completed);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Auto-compaction failed"));
        // The failed compaction left the conversation untouched.
        assert_eq!(test.agent.messages.len(), 16);
        assert_eq!(test.agent.messages[14].role, Role::Tool);
        assert_eq!(test.agent.messages[15].content, "done");
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
