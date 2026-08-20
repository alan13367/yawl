//! The agent loop: send messages → stream → execute tool calls → append
//! results → repeat until the model stops calling tools. Iterations are
//! uncapped; Ctrl+C aborts the in-flight turn, not the process.

use crate::compaction;
use crate::config::Config;
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
    pub config: Config,
    /// Current model spec (may carry an `anthropic:`/`openai:` prefix);
    /// switchable mid-session via `/model`.
    pub model: String,
    pub messages: Vec<Message>,
    pub session: Session,
    /// Last provider-reported total (input + output) tokens — the best
    /// estimate of current context usage.
    pub context_tokens: u64,
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
        self.config.context_window_for(&self.model)
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
        crate::set_interrupted(false);
        if let Some(input) = user_input {
            let msg = Message::user(input);
            self.session.append_message(&msg)?;
            self.messages.push(msg);
        }
        let system = crate::prompt::build_system_prompt();

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

            let (provider, bare_model) = provider::resolve(&self.model, &self.config)?;
            let request = provider::Request {
                model: &bare_model,
                system: &system,
                messages: &self.messages,
                tools: &specs,
                max_tokens: self.config.max_tokens_for(&self.model),
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
        let mut aborted = false;
        for call in calls {
            let result = if aborted || crate::interrupted() {
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
                if crate::interrupted() {
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
            self.config.max_tokens_for(&self.model),
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
