use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Conversation, is_undoable_user_prompt, last_undoable_user_index};
use crate::agent::TurnEvent;
use crate::compaction;
use crate::config::Config;
use crate::error::Error;
use crate::provider::{
    CompactionOutput, Event as ProviderEvent, Message, Provider, Request, Role, TokenUsage,
    ToolCall,
};
use crate::session::Session;

enum ProviderStep {
    Output {
        text: &'static str,
        tool_calls: Vec<ToolCall>,
        input_tokens: u64,
        output_tokens: u64,
    },
    Fail,
    ContextLimit,
}

struct ScriptedProvider {
    steps: Rc<RefCell<VecDeque<ProviderStep>>>,
    requests: Rc<RefCell<Vec<Vec<Role>>>>,
    systems: Option<Rc<RefCell<Vec<String>>>>,
}

impl Provider for ScriptedProvider {
    fn stream_once(
        &self,
        request: &Request<'_>,
        on_event: &mut dyn FnMut(ProviderEvent),
    ) -> Result<(), Error> {
        if let Some(systems) = &self.systems {
            systems.borrow_mut().push(request.system.to_string());
        }
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
                on_event(ProviderEvent::Usage(TokenUsage {
                    input_tokens,
                    output_tokens,
                    cached_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    cache_details_reported: false,
                }));
                on_event(ProviderEvent::Done);
                Ok(())
            }
            ProviderStep::Fail => Err(Error::Protocol("scripted failure".into())),
            ProviderStep::ContextLimit => Err(Error::Http {
                status: 400,
                body: "context_length_exceeded".into(),
            }),
        }
    }
}

struct RemoteCompactionProvider;

impl Provider for RemoteCompactionProvider {
    fn stream_once(
        &self,
        _request: &Request<'_>,
        on_event: &mut dyn FnMut(ProviderEvent),
    ) -> Result<(), Error> {
        on_event(ProviderEvent::TextDelta("portable summary".into()));
        on_event(ProviderEvent::Usage(TokenUsage {
            input_tokens: 20,
            output_tokens: 5,
            ..TokenUsage::default()
        }));
        on_event(ProviderEvent::Done);
        Ok(())
    }

    fn compact(&self, request: &Request<'_>) -> Result<Option<CompactionOutput>, Error> {
        assert_eq!(request.messages.len(), 2);
        Ok(Some(CompactionOutput {
            replacement_history: vec![serde_json::json!({
                "type": "compaction",
                "encrypted_content": "opaque"
            })],
            usage: TokenUsage {
                input_tokens: 30,
                output_tokens: 10,
                ..TokenUsage::default()
            },
        }))
    }
}

struct TestAgent {
    root: PathBuf,
    sessions_dir: PathBuf,
    agent: Conversation,
}

impl TestAgent {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("yawl-agent-{}-{nonce}-{name}", std::process::id()));
        let home_dir = root.join("home");
        let project_dir = root.join("project");
        let config = Config {
            model: Some("test".into()),
            auto_compact: false,
            home_dir: home_dir.clone(),
            project_dir,
            ..Config::test_default()
        };
        let cwd = root.join("cwd");
        let _ = std::fs::create_dir_all(&cwd);
        let dirs = config.session_dirs(&cwd);
        let session =
            Session::create(&dirs.project, &cwd, "test").expect("test session should be created");
        Self {
            root,
            sessions_dir: dirs.project,
            agent: Conversation::persistent(config, "test".into(), session, Vec::new(), cwd),
        }
    }
}

impl Drop for TestAgent {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn active_display_config_sync_keeps_the_status_bar_layout() {
    let mut conversation =
        Conversation::memory(Config::test_default(), "test".into(), "child".into());
    let mut active_config = conversation.config().clone();
    active_config.status_bar.items.clear();
    active_config.status_bar.style = crate::config::StatusBarStyle::Plain;
    active_config.bell = false;

    conversation.sync_display_config(&active_config);

    assert_eq!(conversation.config().status_bar, active_config.status_bar);
    assert!(!conversation.config().bell);
}

#[test]
fn tool_allowlist_filters_child_tool_scans() {
    let test = TestAgent::new("allowlist");
    let mut child =
        Conversation::memory(test.agent.config().clone(), "test".into(), "child".into());
    child.set_tool_allowlist(vec!["read_file".into()]);

    let mut names = child
        .scan_tools()
        .specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(
        names,
        ["read_file"],
        "preset children see only allowed tools"
    );
}

#[test]
fn print_mode_pump_delivers_deferred_results_in_a_follow_up_turn() {
    let mut test = TestAgent::new("subagent-pump");
    test.agent.config.subagents = true;
    test.agent.subagent_manager().push_test_deferred(
        1,
        "scout",
        crate::subagent::RunOutcome::Completed,
        "found it",
    );
    let steps = Rc::new(RefCell::new(VecDeque::from([ProviderStep::Output {
        text: "delivered summary",
        tool_calls: Vec::new(),
        input_tokens: 10,
        output_tokens: 2,
    }])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = |_: &str, _: &Config| {
        Ok::<(Box<dyn Provider>, String), Error>((
            Box::new(ScriptedProvider {
                steps: Rc::clone(&steps),
                requests: Rc::clone(&requests),
                systems: None,
            }),
            "test".into(),
        ))
    };

    // Drive the pump's delivery turn with the injected resolver the same
    // way run drives it with the real one.
    let pumped = test
        .agent
        .pump_subagent_results_with(&mut |_| {}, 1, &mut resolve)
        .expect("pump should complete");

    assert!(pumped);
    assert!(!test.agent.subagent_manager().has_deferred());
    let messages = &test.agent.messages;
    assert_eq!(
        messages
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>(),
        [Role::User, Role::Assistant]
    );
    assert!(
        messages[0].content.contains("Background subagent results"),
        "the delivery message should carry the results; got:\n{}",
        messages[0].content
    );
    assert!(
        messages[0].content.contains("found it"),
        "the delivery message should carry the subagent result text"
    );
    assert_eq!(messages[1].content, "delivered summary");
    assert_eq!(requests.borrow().len(), 1);
}

#[test]
fn print_mode_pump_with_no_active_subagents_returns_promptly() {
    let mut test = TestAgent::new("subagent-pump-idle");
    test.agent.print_mode = true;
    let pumped = test
        .agent
        .pump_subagent_results_with(&mut |_| {}, 1, &mut |_, _| unreachable!())
        .expect("idle pump should complete immediately");
    assert!(pumped);
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
                systems: None,
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
    let (_, replayed) = Session::open(&test.sessions_dir, test.agent.session_id())
        .expect("persisted session should replay");
    assert_eq!(
        replayed
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>(),
        [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );
}

fn run_scripted_init_write(test: &mut TestAgent, content: &str) -> Rc<RefCell<Vec<String>>> {
    let path = test.root.join("cwd/AGENTS.md");
    let steps = Rc::new(RefCell::new(VecDeque::from([
        ProviderStep::Output {
            text: "",
            tool_calls: vec![ToolCall {
                id: "init-write".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({
                    "path": path.to_string_lossy(),
                    "content": content,
                })
                .to_string(),
            }],
            input_tokens: 10,
            output_tokens: 2,
        },
        ProviderStep::Output {
            text: "Updated AGENTS.md.",
            tool_calls: Vec::new(),
            input_tokens: 12,
            output_tokens: 3,
        },
    ])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let systems = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = |_: &str, _: &Config| {
        Ok::<(Box<dyn Provider>, String), Error>((
            Box::new(ScriptedProvider {
                steps: Rc::clone(&steps),
                requests: Rc::clone(&requests),
                systems: Some(Rc::clone(&systems)),
            }),
            "test".into(),
        ))
    };

    let completed = test
        .agent
        .run_init_with("/init".to_string().into(), &mut |_| {}, &mut resolve)
        .expect("init turn should complete");
    assert!(completed);
    systems
}

#[test]
fn init_turn_keeps_literal_message_and_created_file_is_undoable() {
    let mut test = TestAgent::new("init-create");
    let agents = test.root.join("cwd/AGENTS.md");

    let systems = run_scripted_init_write(&mut test, "# Agent guide\n");

    assert_eq!(
        std::fs::read_to_string(&agents).expect("read"),
        "# Agent guide\n"
    );
    assert_eq!(test.agent.messages[0].content, "/init");
    assert_eq!(systems.borrow().len(), 2);
    assert!(
        systems
            .borrow()
            .iter()
            .all(|system| system.contains("<init_task>"))
    );
    let (_, replayed) = Session::open(&test.sessions_dir, test.agent.session_id())
        .expect("init session should replay");
    assert_eq!(replayed[0].content, "/init");

    let report = test.agent.undo_last_turn().expect("undo init");
    assert!(report.restored_files);
    assert!(!agents.exists());
    assert!(test.agent.messages.is_empty());

    let steps = Rc::new(RefCell::new(VecDeque::from([ProviderStep::Output {
        text: "normal reply",
        tool_calls: Vec::new(),
        input_tokens: 5,
        output_tokens: 2,
    }])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let normal_systems = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = |_: &str, _: &Config| {
        Ok::<(Box<dyn Provider>, String), Error>((
            Box::new(ScriptedProvider {
                steps: Rc::clone(&steps),
                requests: Rc::clone(&requests),
                systems: Some(Rc::clone(&normal_systems)),
            }),
            "test".into(),
        ))
    };
    test.agent
        .run_turn_with(Some("next".into()), &mut |_| {}, &mut resolve)
        .expect("normal turn should complete");
    assert!(!normal_systems.borrow()[0].contains("<init_task>"));
}

#[test]
fn init_update_restores_existing_agents_file_on_undo() {
    let mut test = TestAgent::new("init-update");
    let agents = test.root.join("cwd/AGENTS.md");
    std::fs::write(&agents, "old guidance\n").expect("seed AGENTS.md");

    run_scripted_init_write(&mut test, "new guidance\n");
    assert_eq!(
        std::fs::read_to_string(&agents).expect("read"),
        "new guidance\n"
    );

    let report = test.agent.undo_last_turn().expect("undo init update");
    assert!(report.restored_files);
    assert_eq!(
        std::fs::read_to_string(&agents).expect("read"),
        "old guidance\n"
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
                systems: None,
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
                systems: None,
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
fn compaction_persists_and_replays_provider_native_history() {
    let mut test = TestAgent::new("remote-compaction");
    for index in 0..12 {
        test.agent
            .append_input_message(Message::user(format!("history {index}")))
            .expect("history should persist");
    }
    let mut resolve = |_: &str, _: &Config| {
        Ok::<(Box<dyn Provider>, String), Error>((
            Box::new(RemoteCompactionProvider),
            "codex-test".into(),
        ))
    };

    test.agent
        .compact_now_with(&mut |_| {}, &mut resolve)
        .expect("remote compaction should succeed");

    assert_eq!(test.agent.messages.len(), 11);
    assert_eq!(
        test.agent.messages[0].provider_data[0]["type"],
        "compaction"
    );
    assert_eq!(
        test.agent.messages[0].provider_data_model.as_deref(),
        Some("codex-test")
    );
    assert_eq!(test.agent.usage().requests, 2);
    let (session, replayed) = Session::open(&test.sessions_dir, test.agent.session_id())
        .expect("compacted session should replay");
    assert_eq!(replayed[0].provider_data[0]["encrypted_content"], "opaque");
    assert_eq!(session.usage().requests, 2);
    assert_eq!(session.usage().cache_resets, 1);
}

#[test]
fn failed_auto_compaction_warns_and_the_turn_continues() {
    let mut test = TestAgent::new("compact-warning");
    test.agent.config.auto_compact = true;
    test.agent
        .config
        .context_windows
        .insert("test".into(), 100_000);
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
            input_tokens: 100_000,
            output_tokens: 0,
        },
        ProviderStep::Fail,
        ProviderStep::Output {
            text: "done",
            tool_calls: Vec::new(),
            input_tokens: 100_000,
            output_tokens: 0,
        },
    ])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = |_: &str, _: &Config| {
        Ok::<(Box<dyn Provider>, String), Error>((
            Box::new(ScriptedProvider {
                steps: Rc::clone(&steps),
                requests: Rc::clone(&requests),
                systems: None,
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
        message.role == Role::Tool && message.is_error && message.content == "[interrupted by user]"
    }));
    assert_eq!(test.agent.messages[0].tool_call_id.as_deref(), Some("one"));
    assert_eq!(test.agent.messages[1].tool_call_id.as_deref(), Some("two"));
}

#[test]
fn last_undoable_user_skips_summaries_and_subagent_results() {
    let summary = compaction::summary_message("old");
    let subagent = Message::subagent_results(vec![crate::provider::SubagentResult {
        id: "sa-1".into(),
        name: "scout".into(),
        status: "completed".into(),
        run_number: 1,
        content: "ok".into(),
    }]);
    let messages = [
        Message::user("keep"),
        Message::assistant("a".into(), vec![]),
        summary,
        Message::assistant("b".into(), vec![]),
        Message::user("undo-me"),
        Message::assistant("c".into(), vec![]),
        subagent,
    ];
    assert_eq!(last_undoable_user_index(&messages), Some(4));
    assert!(is_undoable_user_prompt(&messages[0]));
    assert!(!is_undoable_user_prompt(&messages[2]));
    assert!(!is_undoable_user_prompt(&messages[6]));
}

#[test]
fn undo_keeps_working_after_compaction_during_a_goal() {
    let mut test = TestAgent::new("undo-compacted-goal");
    test.agent
        .append_input_message(Message::user("earlier prompt"))
        .expect("earlier user message");
    test.agent
        .append_input_message(Message::assistant("earlier answer".into(), vec![]))
        .expect("earlier assistant message");
    test.agent
        .start_goal("finish the feature".to_string().into())
        .expect("goal should start");
    for index in 0..20 {
        test.agent
            .append_input_message(Message::assistant(format!("goal progress {index}"), vec![]))
            .expect("goal progress should persist");
    }

    let protected = last_undoable_user_index(&test.agent.messages);
    let range = compaction::compaction_range(&test.agent.messages, protected);
    let start = range.start;
    let replaced = range.len();
    test.agent
        .persist_compaction("compacted goal progress", start, replaced, &[], None)
        .expect("compaction should persist");
    compaction::apply_summary_range(&mut test.agent.messages, "compacted goal progress", range);

    assert_eq!(last_undoable_user_index(&test.agent.messages), Some(2));
    let report = test.agent.undo_last_turn().expect("undo should succeed");
    assert!(report.dropped > 0);
    assert_eq!(test.agent.messages.len(), 2);
    assert_eq!(test.agent.messages[0].content, "earlier prompt");
    assert_eq!(test.agent.active_goal(), None);
}

#[test]
fn undo_last_turn_drops_the_prompt_and_restores_files() {
    let mut test = TestAgent::new("undo-turn");
    let file = test.root.join("cwd").join("note.txt");
    std::fs::write(&file, "before").expect("write");
    test.agent
        .persistent_mut()
        .checkpoints
        .snapshot()
        .expect("snapshot");
    test.agent
        .persistent_mut()
        .checkpoints
        .remember_path(&file)
        .expect("pre-image");
    test.agent
        .append_input_message(Message::user("edit the file"))
        .expect("user");
    test.agent
        .append_input_message(Message::assistant("done".into(), vec![]))
        .expect("assistant");
    std::fs::write(&file, "after").expect("mutate");

    let report = test.agent.undo_last_turn().expect("undo");
    assert_eq!(report.dropped, 2);
    assert!(test.agent.messages.is_empty());
    assert_eq!(std::fs::read_to_string(&file).expect("read"), "before");
}

#[test]
fn undo_without_a_user_turn_is_a_no_op() {
    let mut test = TestAgent::new("undo-empty");
    let report = test.agent.undo_last_turn().expect("undo");
    assert_eq!(report.dropped, 0);
    assert!(test.agent.messages.is_empty());
}

fn scripted_resolve(
    steps: Rc<RefCell<VecDeque<ProviderStep>>>,
    requests: Rc<RefCell<Vec<Vec<Role>>>>,
) -> impl FnMut(&str, &Config) -> Result<(Box<dyn Provider>, String), Error> {
    move |_: &str, _: &Config| {
        Ok::<(Box<dyn Provider>, String), Error>((
            Box::new(ScriptedProvider {
                steps: Rc::clone(&steps),
                requests: Rc::clone(&requests),
                systems: None,
            }),
            "test".into(),
        ))
    }
}

#[test]
fn goal_mode_continues_on_text_only_then_finishes_with_goal_complete() {
    let mut test = TestAgent::new("goal-complete");
    test.agent
        .start_goal("ship the feature".to_string().into())
        .expect("start goal");
    let steps = Rc::new(RefCell::new(VecDeque::from([
        ProviderStep::Output {
            text: "still working",
            tool_calls: Vec::new(),
            input_tokens: 10,
            output_tokens: 2,
        },
        ProviderStep::Output {
            text: "",
            tool_calls: vec![ToolCall {
                id: "goal-1".into(),
                name: crate::tools::GOAL_COMPLETE_TOOL_NAME.into(),
                arguments: r#"{"result":"shipped"}"#.into(),
            }],
            input_tokens: 10,
            output_tokens: 2,
        },
    ])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));

    let completed = test
        .agent
        .run_goal_with(&mut |_| {}, &mut resolve)
        .expect("goal turn should complete");

    assert!(completed);
    assert_eq!(test.agent.active_goal(), None);
    assert_eq!(test.agent.latest_turn_result(), "shipped");
    assert!(
        test.agent
            .messages
            .iter()
            .any(|message| message.is_hidden_control()),
        "goal continuation should be recorded in history"
    );
    assert_eq!(
        test.agent
            .messages
            .last()
            .map(|message| message.content.as_str()),
        Some("shipped")
    );
}

#[test]
fn ordinary_turn_does_not_resume_a_paused_goal() {
    let mut test = TestAgent::new("goal-paused-normal-turn");
    test.agent
        .start_goal("ship the feature".to_string().into())
        .expect("start goal");
    let steps = Rc::new(RefCell::new(VecDeque::from([ProviderStep::Output {
        text: "answer to the separate question",
        tool_calls: Vec::new(),
        input_tokens: 10,
        output_tokens: 2,
    }])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));

    let completed = test
        .agent
        .run_turn_with(
            Some("a separate question".into()),
            &mut |_| {},
            &mut resolve,
        )
        .expect("ordinary turn should complete");

    assert!(completed);
    assert_eq!(test.agent.active_goal(), Some("ship the feature"));
    assert_eq!(
        test.agent.latest_turn_result(),
        "answer to the separate question"
    );
    assert_eq!(requests.borrow().len(), 1);
    assert_eq!(
        test.agent
            .messages
            .iter()
            .filter(|message| message.control
                == Some(crate::provider::MessageControl::GoalContinuation))
            .count(),
        0
    );
    assert!(
        !test
            .agent
            .scan_tools()
            .specs()
            .iter()
            .any(|tool| tool.name == crate::tools::GOAL_COMPLETE_TOOL_NAME),
        "goal_complete must stay private to an actively running goal"
    );
}

#[test]
fn goal_mode_rejects_mixed_goal_complete_batches() {
    let mut test = TestAgent::new("goal-mixed");
    test.agent
        .start_goal("finish".to_string().into())
        .expect("start goal");
    let steps = Rc::new(RefCell::new(VecDeque::from([
        ProviderStep::Output {
            text: "",
            tool_calls: vec![
                ToolCall {
                    id: "goal-1".into(),
                    name: crate::tools::GOAL_COMPLETE_TOOL_NAME.into(),
                    arguments: r#"{"result":"done"}"#.into(),
                },
                ToolCall {
                    id: "shell-1".into(),
                    name: "shell".into(),
                    arguments: r#"{"command":"printf ok"}"#.into(),
                },
            ],
            input_tokens: 10,
            output_tokens: 2,
        },
        ProviderStep::Output {
            text: "",
            tool_calls: vec![ToolCall {
                id: "goal-2".into(),
                name: crate::tools::GOAL_COMPLETE_TOOL_NAME.into(),
                arguments: r#"{"result":"done"}"#.into(),
            }],
            input_tokens: 10,
            output_tokens: 2,
        },
    ])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));

    let completed = test
        .agent
        .run_goal_with(&mut |_| {}, &mut resolve)
        .expect("goal turn should complete");

    assert!(completed);
    assert_eq!(test.agent.active_goal(), None);
    let first_batch_results = test
        .agent
        .messages
        .iter()
        .filter(|message| {
            message.role == Role::Tool
                && matches!(message.tool_call_id.as_deref(), Some("goal-1" | "shell-1"))
        })
        .map(|message| message.tool_call_id.as_deref().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(first_batch_results, ["goal-1", "shell-1"]);
    assert!(
        test.agent.messages.iter().any(|message| {
            message.role == Role::Tool
                && message.tool_name.as_deref() == Some(crate::tools::GOAL_COMPLETE_TOOL_NAME)
                && message.is_error
                && message.content.contains("only tool call")
        }),
        "mixed completion should be rejected before a later success"
    );
}

#[test]
fn planning_requires_three_questions_then_persists_the_completed_plan() {
    let mut test = TestAgent::new("plan-complete");
    test.agent.enable_interactive_questions();
    test.agent
        .start_plan("add planning mode".to_string().into())
        .expect("start plan");
    let question_arguments = serde_json::json!({
        "questions": (1..=3).map(|index| serde_json::json!({
            "id": format!("q{index}"),
            "question": format!("Choice {index}?"),
            "options": [
                {"label": "Recommended", "description": "use the default"},
                {"label": "Alternative", "description": "use another approach"}
            ],
            "recommended": 0
        })).collect::<Vec<_>>()
    })
    .to_string();
    let steps = Rc::new(RefCell::new(VecDeque::from([
        ProviderStep::Output {
            text: "",
            tool_calls: vec![ToolCall {
                id: "questions-invalid".into(),
                name: crate::tools::USER_INPUT_TOOL_NAME.into(),
                arguments: serde_json::json!({
                    "questions": [
                        {"id":"one","question":"One?","options":[{"label":"A","description":"a"},{"label":"B","description":"b"}],"recommended":0},
                        {"id":"two","question":"Two?","options":[{"label":"A","description":"a"},{"label":"B","description":"b"}],"recommended":0}
                    ]
                })
                .to_string(),
            }],
            input_tokens: 10,
            output_tokens: 2,
        },
        ProviderStep::Output {
            text: "",
            tool_calls: vec![ToolCall {
                id: "questions-1".into(),
                name: crate::tools::USER_INPUT_TOOL_NAME.into(),
                arguments: question_arguments,
            }],
            input_tokens: 10,
            output_tokens: 2,
        },
        ProviderStep::Output {
            text: "",
            tool_calls: vec![ToolCall {
                id: "plan-1".into(),
                name: crate::tools::PLAN_COMPLETE_TOOL_NAME.into(),
                arguments: r##"{"plan":"# Plan\n\n1. Build it."}"##.into(),
            }],
            input_tokens: 10,
            output_tokens: 2,
        },
    ])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));
    let broker = test.agent.question_broker();
    let responder = std::thread::spawn(move || {
        let mut answered = 0;
        while answered < 3 {
            if let Some(snapshot) = broker.snapshot() {
                broker
                    .answer(snapshot.request_id, snapshot.question.recommended)
                    .expect("answer question");
                answered += 1;
            } else {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    });

    let completed = test
        .agent
        .run_plan_with(&mut |_| {}, &mut resolve)
        .expect("planning should complete");
    responder.join().expect("question responder");

    assert!(completed);
    assert!(test.agent.messages.iter().any(|message| {
        message.tool_call_id.as_deref() == Some("questions-invalid")
            && message.is_error
            && message.content.contains("exactly three")
    }));
    assert_eq!(test.agent.active_plan(), Some("# Plan\n\n1. Build it."));
    assert!(matches!(
        test.agent.plan_state(),
        Some(crate::session::PlanState::Ready { .. })
    ));

    let failed_steps = Rc::new(RefCell::new(VecDeque::from([ProviderStep::Fail])));
    let mut failed_resolve = scripted_resolve(failed_steps, Rc::new(RefCell::new(Vec::new())));
    assert!(
        test.agent
            .run_plan_implementation_with(
                Some("implement it".to_string().into()),
                &mut |_| {},
                &mut failed_resolve,
            )
            .is_err()
    );
    assert_eq!(
        test.agent.active_plan(),
        Some("# Plan\n\n1. Build it."),
        "implementation errors must retain the active plan"
    );
}

#[test]
fn unrelated_follow_up_replies_normally_and_leaves_the_plan_active() {
    let mut test = TestAgent::new("plan-followup");
    test.agent.enable_interactive_questions();
    test.agent
        .start_plan("add planning mode".to_string().into())
        .expect("start plan");
    let question_arguments = serde_json::json!({
        "questions": (1..=3).map(|index| serde_json::json!({
            "id": format!("q{index}"),
            "question": format!("Choice {index}?"),
            "options": [
                {"label": "Recommended", "description": "use the default"},
                {"label": "Alternative", "description": "use another approach"}
            ],
            "recommended": 0
        })).collect::<Vec<_>>()
    })
    .to_string();
    let steps = Rc::new(RefCell::new(VecDeque::from([
        ProviderStep::Output {
            text: "",
            tool_calls: vec![ToolCall {
                id: "questions-1".into(),
                name: crate::tools::USER_INPUT_TOOL_NAME.into(),
                arguments: question_arguments,
            }],
            input_tokens: 10,
            output_tokens: 2,
        },
        ProviderStep::Output {
            text: "",
            tool_calls: vec![ToolCall {
                id: "plan-1".into(),
                name: crate::tools::PLAN_COMPLETE_TOOL_NAME.into(),
                arguments: r##"{"plan":"# Plan\n\n1. Build it."}"##.into(),
            }],
            input_tokens: 10,
            output_tokens: 2,
        },
    ])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));
    let broker = test.agent.question_broker();
    let responder = std::thread::spawn(move || {
        let mut answered = 0;
        while answered < 3 {
            if let Some(snapshot) = broker.snapshot() {
                broker
                    .answer(snapshot.request_id, snapshot.question.recommended)
                    .expect("answer question");
                answered += 1;
            } else {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    });

    let completed = test
        .agent
        .run_plan_with(&mut |_| {}, &mut resolve)
        .expect("planning should complete");
    responder.join().expect("question responder");

    assert!(completed);
    assert!(test.agent.plan_ready_this_turn());
    assert_eq!(test.agent.active_plan(), Some("# Plan\n\n1. Build it."));

    let reply_steps = Rc::new(RefCell::new(VecDeque::from([ProviderStep::Output {
        text: "This plan adds a planning mode before implementation starts.",
        tool_calls: Vec::new(),
        input_tokens: 10,
        output_tokens: 2,
    }])));
    let mut reply_resolve = scripted_resolve(reply_steps, Rc::new(RefCell::new(Vec::new())));

    let completed = test
        .agent
        .run_plan_follow_up_with(
            "what is this plan about?".to_string().into(),
            &mut |_| {},
            &mut reply_resolve,
        )
        .expect("unrelated follow-up should complete");

    assert!(completed);
    assert!(
        !test.agent.plan_ready_this_turn(),
        "a text reply must not offer the plan handoff picker"
    );
    assert_eq!(test.agent.active_plan(), Some("# Plan\n\n1. Build it."));
    assert_eq!(
        test.agent
            .messages
            .last()
            .map(|message| message.content.as_str()),
        Some("This plan adds a planning mode before implementation starts."),
        "a text reply ends the turn without a plan continuation"
    );
}

#[test]
fn resumed_plan_remembers_that_requirements_were_answered() {
    let mut test = TestAgent::new("plan-question-resume");
    test.agent.enable_interactive_questions();
    test.agent
        .start_plan("add planning mode".to_string().into())
        .expect("start plan");
    let question_arguments = serde_json::json!({
        "questions": (1..=3).map(|index| serde_json::json!({
            "id": format!("q{index}"),
            "question": format!("Choice {index}?"),
            "options": [
                {"label": "Recommended", "description": "use the default"},
                {"label": "Alternative", "description": "use another approach"}
            ],
            "recommended": 0
        })).collect::<Vec<_>>()
    })
    .to_string();
    let first_steps = Rc::new(RefCell::new(VecDeque::from([
        ProviderStep::Output {
            text: "",
            tool_calls: vec![ToolCall {
                id: "questions-1".into(),
                name: crate::tools::USER_INPUT_TOOL_NAME.into(),
                arguments: question_arguments,
            }],
            input_tokens: 10,
            output_tokens: 2,
        },
        ProviderStep::Fail,
    ])));
    let mut first_resolve = scripted_resolve(first_steps, Rc::new(RefCell::new(Vec::new())));
    let broker = test.agent.question_broker();
    let responder = std::thread::spawn(move || {
        let mut answered = 0;
        while answered < 3 {
            if let Some(snapshot) = broker.snapshot() {
                broker
                    .answer(snapshot.request_id, snapshot.question.recommended)
                    .expect("answer question");
                answered += 1;
            } else {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    });

    assert!(
        test.agent
            .run_plan_with(&mut |_| {}, &mut first_resolve)
            .is_err()
    );
    responder.join().expect("question responder");
    assert!(
        test.agent
            .plan_state()
            .is_some_and(crate::session::PlanState::questions_asked)
    );

    let resumed_steps = Rc::new(RefCell::new(VecDeque::from([ProviderStep::Output {
        text: "",
        tool_calls: vec![ToolCall {
            id: "plan-1".into(),
            name: crate::tools::PLAN_COMPLETE_TOOL_NAME.into(),
            arguments: r##"{"plan":"# Plan\n\n1. Build it."}"##.into(),
        }],
        input_tokens: 10,
        output_tokens: 2,
    }])));
    let mut resumed_resolve = scripted_resolve(resumed_steps, Rc::new(RefCell::new(Vec::new())));
    assert!(
        test.agent
            .run_plan_with(&mut |_| {}, &mut resumed_resolve)
            .expect("resumed plan should complete without another question batch")
    );
    assert_eq!(test.agent.active_plan(), Some("# Plan\n\n1. Build it."));
}

#[test]
fn unrelated_plan_follow_up_transitions_to_the_full_tool_registry() {
    let mut test = TestAgent::new("plan-unrelated-tools");
    test.agent
        .start_plan("add planning mode".to_string().into())
        .expect("start plan");
    let ready = Message::assistant("# Plan".into(), vec![]);
    test.agent
        .persistent_mut()
        .session
        .append_plan_ready("# Plan", &ready)
        .expect("ready plan");
    test.agent.messages.push(ready);
    let output_path = test.root.join("unrelated.txt");
    let steps = Rc::new(RefCell::new(VecDeque::from([
        ProviderStep::Output {
            text: "",
            tool_calls: vec![ToolCall {
                id: "classify-1".into(),
                name: crate::tools::PLAN_ACTION_TOOL_NAME.into(),
                arguments: r#"{"action":"unrelated"}"#.into(),
            }],
            input_tokens: 10,
            output_tokens: 2,
        },
        ProviderStep::Output {
            text: "",
            tool_calls: vec![ToolCall {
                id: "write-1".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({
                    "path": output_path,
                    "content": "done"
                })
                .to_string(),
            }],
            input_tokens: 10,
            output_tokens: 2,
        },
        ProviderStep::Output {
            text: "Completed the unrelated request.",
            tool_calls: Vec::new(),
            input_tokens: 10,
            output_tokens: 2,
        },
    ])));
    let mut resolve = scripted_resolve(steps, Rc::new(RefCell::new(Vec::new())));

    assert!(
        test.agent
            .run_plan_follow_up_with(
                "write an unrelated file".to_string().into(),
                &mut |_| {},
                &mut resolve,
            )
            .expect("unrelated request should run normally")
    );
    assert_eq!(
        std::fs::read_to_string(&output_path).expect("unrelated write should run"),
        "done"
    );
    assert_eq!(test.agent.active_plan(), Some("# Plan"));
}

#[test]
fn plan_implementation_receives_deferred_subagent_results() {
    let mut test = TestAgent::new("plan-subagent-results");
    test.agent.config.subagents = true;
    test.agent
        .start_plan("add planning mode".to_string().into())
        .expect("start plan");
    let ready = Message::assistant("# Plan".into(), vec![]);
    test.agent
        .persistent_mut()
        .session
        .append_plan_ready("# Plan", &ready)
        .expect("ready plan");
    test.agent.messages.push(ready);
    test.agent.subagent_manager().push_test_deferred(
        1,
        "worker",
        crate::subagent::RunOutcome::Completed,
        "child result",
    );
    let steps = Rc::new(RefCell::new(VecDeque::from([ProviderStep::Output {
        text: "",
        tool_calls: vec![ToolCall {
            id: "implemented-1".into(),
            name: crate::tools::PLAN_IMPLEMENTED_TOOL_NAME.into(),
            arguments: r#"{"result":"Implemented the plan."}"#.into(),
        }],
        input_tokens: 10,
        output_tokens: 2,
    }])));
    steps.borrow_mut().push_front(ProviderStep::Output {
        text: "Implement the agreed feature.",
        tool_calls: vec![],
        input_tokens: 10,
        output_tokens: 2,
    });
    let mut resolve = scripted_resolve(steps, Rc::new(RefCell::new(Vec::new())));

    assert!(
        test.agent
            .run_plan_implementation_with(
                Some("implement it".to_string().into()),
                &mut |_| {},
                &mut resolve,
            )
            .expect("implementation should complete")
    );
    assert!(test.agent.messages.iter().any(|message| {
        message
            .subagent_results
            .iter()
            .any(|result| result.content == "child result")
    }));
    assert_eq!(test.agent.active_plan(), None);
}

#[test]
fn steering_skips_pending_tools_and_injects_the_steer_message() {
    let mut test = TestAgent::new("steer-tools");
    test.agent.steer_inbox().push(crate::provider::TurnInput {
        text: "stop and summarize".into(),
        images: Vec::new(),
    });
    let steps = Rc::new(RefCell::new(VecDeque::from([
        ProviderStep::Output {
            text: "working",
            tool_calls: vec![ToolCall {
                id: "shell-1".into(),
                name: "shell".into(),
                arguments: r#"{"command":"printf tool-output"}"#.into(),
            }],
            input_tokens: 10,
            output_tokens: 2,
        },
        ProviderStep::Output {
            text: "summary",
            tool_calls: Vec::new(),
            input_tokens: 10,
            output_tokens: 2,
        },
    ])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));
    let mut steer_accepted = false;

    let completed = test
        .agent
        .run_turn_with(
            Some("start".into()),
            &mut |event| {
                if matches!(event, TurnEvent::SteerAccepted { .. }) {
                    steer_accepted = true;
                }
            },
            &mut resolve,
        )
        .expect("steered turn should complete");

    assert!(completed);
    assert!(
        test.agent.messages.iter().any(|message| {
            message.role == Role::Tool
                && message.content == "[skipped because the user steered]"
                && message.is_error
        }),
        "pending tools should be skipped once steering is accepted"
    );
    assert!(
        test.agent
            .messages
            .iter()
            .any(|message| message.is_steering() && message.content == "stop and summarize"),
        "accepted steer should be recorded as a steering user message"
    );
    assert!(steer_accepted, "steer acceptance should surface to the UI");
}

fn reopen_test_agent(test: &mut TestAgent) {
    let (session, messages) = Session::open(&test.sessions_dir, test.agent.session_id()).unwrap();
    test.agent = Conversation::persistent(
        test.agent.config.clone(),
        "test".into(),
        session,
        messages,
        test.root.join("cwd"),
    );
}

fn two_checkpoint_turns(test: &mut TestAgent) -> PathBuf {
    let path = test.root.join("cwd/undo.txt");
    std::fs::write(&path, "original").unwrap();
    for (prompt, contents) in [("first", "first edit"), ("second", "second edit")] {
        test.agent.checkpoint_snapshot().unwrap().unwrap();
        test.agent
            .append_input_message(Message::user(prompt))
            .unwrap();
        test.agent.checkpoint_path(&path).unwrap().unwrap();
        std::fs::write(&path, contents).unwrap();
    }
    path
}

#[test]
fn undo_retries_the_same_checkpoint_after_history_write_fails() {
    for reopen in [false, true] {
        let mut test = TestAgent::new("undo-write-failure");
        let path = two_checkpoint_turns(&mut test);
        // No files change unless the intent was saved successfully.
        test.agent.persistent_mut().session.fail_append_after(0);
        assert!(test.agent.undo_last_turn().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second edit");
        assert!(
            test.agent
                .persistent_state()
                .session
                .pending_undo()
                .is_none()
        );
        // The intent persists, files restore, then the history append fails.
        test.agent.persistent_mut().session.fail_append_after(1);
        assert!(test.agent.undo_last_turn().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first edit");
        assert_eq!(test.agent.messages.len(), 2);
        assert_eq!(
            test.agent.persistent_state().checkpoints.last_index(),
            Some(1)
        );
        if reopen {
            reopen_test_agent(&mut test);
        }
        assert_eq!(test.agent.undo_last_turn().unwrap().dropped, 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first edit");
        assert_eq!(test.agent.messages.len(), 1);
        assert_eq!(
            test.agent.persistent_state().checkpoints.last_index(),
            Some(0)
        );
        test.agent.undo_last_turn().unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "original");
    }
}

#[test]
fn undo_recovery_after_cleanup_does_not_restore_an_older_turn() {
    let mut test = TestAgent::new("undo-finish-failure");
    let path = two_checkpoint_turns(&mut test);
    // Intent and history commit; the checkpoint is removed, but finishing fails.
    test.agent.persistent_mut().session.fail_append_after(2);
    assert!(test.agent.undo_last_turn().is_err());
    assert_eq!(test.agent.messages.len(), 1);
    reopen_test_agent(&mut test);
    let report = test.agent.undo_last_turn().unwrap();
    assert_eq!(report.dropped, 1);
    assert!(report.restored_files);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "first edit");
    assert_eq!(test.agent.messages.len(), 1);
    assert!(
        test.agent
            .persistent_state()
            .session
            .pending_undo()
            .is_none()
    );
}

#[test]
fn undo_recovery_after_checkpoint_cleanup_failure_is_idempotent() {
    let mut test = TestAgent::new("undo-cleanup-failure");
    let path = two_checkpoint_turns(&mut test);
    let temporary = test
        .agent
        .config
        .home_dir
        .join("checkpoints")
        .join(test.agent.session_id())
        .join("stack.json.tmp");
    std::fs::create_dir(&temporary).unwrap();
    assert!(test.agent.undo_last_turn().is_err());
    assert_eq!(test.agent.messages.len(), 1);
    std::fs::remove_dir(&temporary).unwrap();
    reopen_test_agent(&mut test);
    test.agent.undo_last_turn().unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "first edit");
    assert_eq!(
        test.agent.persistent_state().checkpoints.last_index(),
        Some(0)
    );
}

#[test]
fn failed_tool_result_is_saved_on_retry_without_rerunning_tools() {
    let mut test = TestAgent::new("tool-storage-failure");
    let path = test.root.join("cwd/tool.txt");
    let calls = vec![
        ToolCall {
            id: "first".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": path, "content": "executed once"}).to_string(),
        },
        ToolCall {
            id: "second".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": path, "content": "must not execute"}).to_string(),
        },
    ];
    test.agent
        .append_input_message(Message::assistant("".into(), calls.clone()))
        .unwrap();
    let registry = test.agent.scan_tools();
    test.agent.persistent_mut().session.fail_append_after(0);
    assert!(
        test.agent
            .run_tools_while(&registry, &calls, &mut |_| {}, || false)
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "executed once");
    let actual_output = test.agent.pending_tool_results[0].content.clone();
    assert!(!test.agent.pending_tool_results[0].is_error);
    // Prove recovery does not execute the first tool again either.
    std::fs::write(&path, "external change").unwrap();
    test.agent.recover_history().unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "external change");
    assert_eq!(test.agent.messages[1].content, actual_output);
    assert!(test.agent.messages[2].content.contains("not executed"));
    reopen_test_agent(&mut test);
    assert_eq!(test.agent.messages.len(), 3);
    assert_eq!(test.agent.messages[1].content, actual_output);
}

#[test]
fn resume_repairs_missing_results_without_executing_uncertain_calls() {
    let mut test = TestAgent::new("missing-results");
    let calls = ["done", "unknown"].map(|id| ToolCall {
        id: id.into(),
        name: "shell".into(),
        arguments: "{}".into(),
    });
    test.agent
        .append_input_message(Message::assistant("".into(), calls.to_vec()))
        .unwrap();
    test.agent
        .append_input_message(Message::tool_result(
            "done",
            "shell",
            "known output".into(),
            false,
        ))
        .unwrap();
    // Also cover old sessions with a user message after the incomplete batch.
    test.agent
        .append_input_message(Message::user("continue"))
        .unwrap();
    reopen_test_agent(&mut test);
    assert_eq!(test.agent.messages.len(), 4);
    assert_eq!(test.agent.messages[1].content, "known output");
    assert_eq!(
        test.agent.messages[2].tool_call_id.as_deref(),
        Some("unknown")
    );
    assert!(test.agent.messages[2].content.contains("may have executed"));
    assert_eq!(test.agent.messages[3].content, "continue");
    reopen_test_agent(&mut test);
    assert_eq!(test.agent.messages.len(), 4);
}

#[test]
fn context_metadata_survives_resume_and_counts_new_content() {
    let mut test = TestAgent::new("context-replay");
    test.agent
        .append_input_message(Message::user("start"))
        .unwrap();
    test.agent.record_context(1000, 50).unwrap();
    test.agent
        .append_input_message(Message::assistant("answer".into(), vec![]))
        .unwrap();
    reopen_test_agent(&mut test);
    assert_eq!(test.agent.estimated_context(50), 1000);
    test.agent
        .append_input_message(Message::user("x".repeat(3000)))
        .unwrap();
    assert_eq!(test.agent.estimated_context(50), 2008);
    assert_eq!(test.agent.estimated_context(150), 2108);
    test.agent.switch_model("other".into()).unwrap();
    assert!(test.agent.estimated_context(50) < 2008);
}

#[test]
fn connect_default_model_is_recorded_in_the_session() {
    let mut test = TestAgent::new("connect-default-model");
    test.agent
        .append_input_message(Message::user("start"))
        .unwrap();
    let plan = crate::onboarding::provider::ConnectionPlan {
        changes: Vec::new(),
        model: "openai:gpt-4.1".into(),
        activation: crate::onboarding::provider::ConnectionActivation::Default,
        provider_label: "OpenAI".into(),
    };

    test.agent
        .change_global_config_batch(plan.changes_for_save())
        .unwrap();
    assert_eq!(test.agent.model(), "openai:gpt-4.1");
    assert_eq!(test.agent.config().model.as_deref(), Some("openai:gpt-4.1"));

    // The switch must be in the log so resuming continues with this model.
    let (session, _) = Session::open(&test.sessions_dir, test.agent.session_id()).unwrap();
    assert_eq!(session.model(), "openai:gpt-4.1");
}

#[test]
fn switched_models_are_recorded_and_adopted_on_resume() {
    let mut test = TestAgent::new("model-resume");
    test.agent
        .append_input_message(Message::user("start"))
        .unwrap();
    test.agent.switch_model("other-model".into()).unwrap();
    assert_eq!(test.agent.model(), "other-model");

    // The switch is in the log, so a replayed session reports it.
    let id = test.agent.session_id().to_string();
    let (session, messages) = Session::open(&test.sessions_dir, &id).unwrap();
    assert_eq!(session.model(), "other-model");
    assert_eq!(messages.len(), 1);
    drop(session);

    // `/resume` continues with that model rather than the configured one.
    let cwd = crate::config::working_dir();
    let dirs = test.agent.config.session_dirs(&cwd);
    std::fs::create_dir_all(&dirs.project).unwrap();
    // `/resume` searches every project directory, so move the log rather than
    // duplicating it into a second one.
    std::fs::rename(
        test.sessions_dir.join(format!("{id}.jsonl")),
        dirs.project.join(format!("{id}.jsonl")),
    )
    .unwrap();
    let mut resumed = Conversation::persistent(
        test.agent.config.clone(),
        "test".into(),
        Session::create(&dirs.project, &cwd, "test").unwrap(),
        Vec::new(),
        cwd.clone(),
    );
    resumed.load_session(&id).unwrap();
    assert_eq!(resumed.model(), "other-model");
    assert_eq!(resumed.messages().len(), 1);
}

fn text_step(text: &'static str) -> ProviderStep {
    ProviderStep::Output {
        text,
        tool_calls: vec![],
        input_tokens: 100,
        output_tokens: 1,
    }
}

#[test]
fn context_limit_gets_at_most_one_compaction_retry() {
    for (enabled, repeat_error) in [(true, false), (true, true), (false, false)] {
        let mut test = TestAgent::new("context-retry");
        test.agent.config.auto_compact = enabled;
        for _ in 0..12 {
            test.agent
                .append_input_message(Message::user("old history"))
                .unwrap();
        }
        let steps = Rc::new(RefCell::new(VecDeque::from([
            ProviderStep::ContextLimit,
            text_step("portable summary"),
            if repeat_error {
                ProviderStep::ContextLimit
            } else {
                text_step("done")
            },
        ])));
        let requests = Rc::new(RefCell::new(Vec::new()));
        let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));
        let mut compactions = 0;
        let result = test.agent.run_turn_with(
            Some("continue".into()),
            &mut |event| {
                if matches!(event, TurnEvent::Compacted { .. }) {
                    compactions += 1;
                }
            },
            &mut resolve,
        );
        assert_eq!(result.is_ok(), enabled && !repeat_error);
        assert_eq!(compactions, usize::from(enabled));
        assert_eq!(requests.borrow().len(), if enabled { 3 } else { 1 });
        if enabled {
            assert_eq!(
                test.agent.usage().requests,
                if repeat_error { 1 } else { 2 }
            );
        }
    }
}

#[test]
fn legacy_resumed_history_is_compacted_before_the_first_request() {
    let mut test = TestAgent::new("legacy-context");
    test.agent.config.auto_compact = true;
    test.agent
        .config
        .context_windows
        .insert("test".into(), 20_000);
    for _ in 0..12 {
        test.agent
            .append_input_message(Message::user("x".repeat(6000)))
            .unwrap();
    }
    reopen_test_agent(&mut test);
    assert!(test.agent.context_usage.is_none());
    let steps = Rc::new(RefCell::new(VecDeque::from([
        text_step("summary"),
        text_step("done"),
    ])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve(steps, Rc::clone(&requests));
    assert!(
        test.agent
            .run_turn_with(Some("continue".into()), &mut |_| {}, &mut resolve)
            .unwrap()
    );
    assert_eq!(
        requests.borrow()[0].len(),
        1,
        "first request must be the summarizer"
    );
    assert_eq!(test.agent.usage().cache_resets, 1);
}

#[test]
fn fresh_tool_output_triggers_compaction_even_when_reported_usage_was_low() {
    let mut test = TestAgent::new("tool-context");
    test.agent.config.auto_compact = true;
    test.agent
        .config
        .context_windows
        .insert("test".into(), 20_000);
    let path = test.root.join("large.txt");
    std::fs::write(&path, "x".repeat(60_000)).unwrap();
    for _ in 0..12 {
        test.agent
            .append_input_message(Message::user("old history"))
            .unwrap();
    }
    let steps = Rc::new(RefCell::new(VecDeque::from([
        ProviderStep::Output {
            text: "",
            tool_calls: vec![ToolCall {
                id: "read".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": path}).to_string(),
            }],
            input_tokens: 100,
            output_tokens: 10,
        },
        text_step("summary"),
        text_step("done"),
    ])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve(steps, Rc::clone(&requests));
    assert!(
        test.agent
            .run_turn_with(Some("read the file".into()), &mut |_| {}, &mut resolve)
            .unwrap()
    );
    assert_eq!(
        requests.borrow()[1].len(),
        1,
        "summarize after the large result"
    );
    assert_eq!(test.agent.usage().cache_resets, 1);
}

#[test]
fn failed_assistant_append_does_not_hide_the_next_user_inputs_context_cost() {
    let mut test = TestAgent::new("context-missing-assistant");
    test.agent
        .append_input_message(Message::user("start"))
        .unwrap();
    test.agent.record_context(1000, 50).unwrap();
    // The response's context measurement persisted, but its assistant did not.
    reopen_test_agent(&mut test);
    test.agent
        .append_input_message(Message::user("x".repeat(3000)))
        .unwrap();
    assert_eq!(test.agent.estimated_context(50), 2008);
}

#[test]
fn deferred_delivery_is_restored_after_a_partial_append_failure() {
    let mut test = TestAgent::new("deferred-storage-failure");
    test.agent.subagent_manager().push_test_deferred(
        1,
        "scout",
        crate::subagent::RunOutcome::Completed,
        "found it",
    );
    let steps = Rc::new(RefCell::new(VecDeque::from([text_step("summary")])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve(steps, Rc::clone(&requests));
    test.agent.persistent_mut().session.fail_append_after(0);
    assert!(
        test.agent
            .run_deferred_subagent_results_with(&mut |_| {}, &mut resolve)
            .is_err()
    );
    assert!(test.agent.subagent_manager().has_deferred());
    assert!(requests.borrow().is_empty());
    assert_eq!(
        test.agent
            .run_deferred_subagent_results_with(&mut |_| {}, &mut resolve)
            .unwrap(),
        Some(true)
    );
    assert!(!test.agent.subagent_manager().has_deferred());
    reopen_test_agent(&mut test);
    assert_eq!(test.agent.messages.len(), 2);
    assert_eq!(test.agent.messages[0].subagent_results.len(), 1);
}

type HandoffRequests = Rc<RefCell<Vec<(String, Vec<Message>)>>>;

struct HandoffProvider {
    requests: HandoffRequests,
    classify: bool,
}

impl Provider for HandoffProvider {
    fn stream_once(
        &self,
        request: &Request<'_>,
        sink: &mut dyn FnMut(ProviderEvent),
    ) -> Result<(), Error> {
        self.requests
            .borrow_mut()
            .push((request.system.into(), request.messages.to_vec()));
        if request.tools.is_empty() {
            sink(ProviderEvent::TextDelta(
                "Keep the public API stable. Earlier checks passed.".into(),
            ));
        } else if self.classify && request.system.contains("phase=\"follow_up\"") {
            sink(ProviderEvent::ToolCall(ToolCall {
                id: "classify".into(),
                name: crate::tools::PLAN_ACTION_TOOL_NAME.into(),
                arguments: r#"{"action":"implement"}"#.into(),
            }));
        } else {
            sink(ProviderEvent::ToolCall(ToolCall {
                id: "implemented".into(),
                name: crate::tools::PLAN_IMPLEMENTED_TOOL_NAME.into(),
                arguments: r#"{"result":"Done."}"#.into(),
            }));
        }
        sink(ProviderEvent::Usage(TokenUsage {
            input_tokens: 25,
            output_tokens: 10,
            ..TokenUsage::default()
        }));
        sink(ProviderEvent::Done);
        Ok(())
    }
}

fn ready_handoff_fixture(name: &str) -> TestAgent {
    let mut test = TestAgent::new(name);
    test.agent
        .start_plan("Keep the public API stable".to_string().into())
        .unwrap();
    let investigation = Message::assistant("large investigation output ".repeat(4000), vec![]);
    test.agent.persist_message(&investigation).unwrap();
    test.agent.messages.push(investigation);
    let plan = "# Unique implementation plan\n\nUpdate parsing and run checks.";
    let ready = Message::assistant(plan.into(), vec![]);
    test.agent
        .persistent_mut()
        .session
        .append_plan_ready(plan, &ready)
        .unwrap();
    test.agent.messages.push(ready);
    test
}

#[test]
fn plan_handoff_reduces_requests_for_picker_and_classified_follow_up() -> Result<(), Error> {
    for classify in [false, true] {
        let mut test = ready_handoff_fixture("handoff-requests");
        let before = test
            .agent
            .messages
            .iter()
            .map(super::context::message_tokens)
            .sum::<u64>();
        let captures = Rc::new(RefCell::new(Vec::new()));
        let mut resolve = |_: &str, _: &Config| -> Result<(Box<dyn Provider>, String), Error> {
            Ok((
                Box::new(HandoffProvider {
                    requests: captures.clone(),
                    classify,
                }),
                "test".into(),
            ))
        };
        if classify {
            test.agent.run_plan_follow_up_with(
                "Implement, preserving compatibility".to_string().into(),
                &mut |_| {},
                &mut resolve,
            )?;
        } else {
            test.agent
                .run_plan_implementation_with(None, &mut |_| {}, &mut resolve)?;
        }
        let requests = captures.borrow();
        assert_eq!(requests.len(), if classify { 3 } else { 2 });
        let (system, messages) = requests.last().unwrap();
        assert!(!system.contains("# Unique implementation plan"));
        assert!(system.contains("read the saved plan file"));
        let text = messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text.matches("# Unique implementation plan").count(), 1);
        assert!(text.contains("Keep the public API stable"));
        assert!(!text.contains("large investigation output"));
        assert!(text.contains(if classify {
            "Implement, preserving compatibility"
        } else {
            "Implement the active plan now."
        }));
        assert!(
            messages
                .iter()
                .map(super::context::message_tokens)
                .sum::<u64>()
                < before / 10
        );
        assert!(crate::session::missing_tool_results(messages).is_none());
        if classify {
            assert!(
                messages
                    .iter()
                    .any(|message| message.tool_call_id.as_deref() == Some("classify"))
            );
        }
        drop(requests);
        // Undo implementation retains the compact handoff and its ready plan.
        test.agent.undo_last_turn()?;
        assert!(test.agent.active_plan().is_some());
        assert!(test.agent.persistent_state().session.plan_has_handoff(1));
        let id = test.agent.session_id().to_string();
        let (session, messages) = Session::open(&test.sessions_dir, &id)?;
        *test.agent.persistent_mut().session = session;
        test.agent.messages = messages;
        captures.borrow_mut().clear();
        test.agent
            .run_plan_implementation_with(None, &mut |_| {}, &mut resolve)?;
        assert_eq!(
            captures.borrow().len(),
            1,
            "resume after undo must not summarize again"
        );
        let log = std::fs::read_to_string(test.sessions_dir.join(format!("{id}.jsonl")))?;
        assert!(
            log.contains("large investigation output"),
            "original history remains on disk"
        );
    }
    Ok(())
}

#[test]
fn failed_plan_handoff_preserves_history_and_retries() -> Result<(), Error> {
    for fail_storage in [false, true] {
        let mut test = ready_handoff_fixture("handoff-failure");
        test.agent
            .append_input_message(Message::user("Implement with care"))?;
        let before = serde_json::to_value(&test.agent.messages)?;
        let captures = Rc::new(RefCell::new(Vec::new()));
        let mut resolve = |_: &str, _: &Config| -> Result<(Box<dyn Provider>, String), Error> {
            Ok((
                Box::new(HandoffProvider {
                    requests: captures.clone(),
                    classify: false,
                }),
                "test".into(),
            ))
        };
        if fail_storage {
            // Usage is saved first; then the compaction append fails.
            test.agent.persistent_mut().session.fail_append_after(1);
            assert!(
                test.agent
                    .prepare_plan_handoff(&mut |_| {}, &mut resolve)
                    .is_err()
            );
        } else {
            let steps = Rc::new(RefCell::new(VecDeque::from([ProviderStep::Fail])));
            let mut failed = scripted_resolve(steps, Rc::new(RefCell::new(Vec::new())));
            assert!(
                test.agent
                    .prepare_plan_handoff(&mut |_| {}, &mut failed)
                    .is_err()
            );
        }
        assert_eq!(serde_json::to_value(&test.agent.messages)?, before);
        assert!(!test.agent.persistent_state().session.plan_has_handoff(1));
        assert!(test.agent.active_plan().is_some());
        test.agent.prepare_plan_handoff(&mut |_| {}, &mut resolve)?;
        assert_eq!(test.agent.messages.len(), 2);
        assert!(test.agent.persistent_state().session.plan_has_handoff(1));
    }
    Ok(())
}

#[test]
fn plan_handoff_survives_compaction_and_new_revision_gets_a_new_handoff() -> Result<(), Error> {
    let mut test = ready_handoff_fixture("handoff-revision");
    test.agent
        .append_input_message(Message::user("Implement"))?;
    let captures = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = |_: &str, _: &Config| -> Result<(Box<dyn Provider>, String), Error> {
        Ok((
            Box::new(HandoffProvider {
                requests: captures.clone(),
                classify: false,
            }),
            "test".into(),
        ))
    };
    test.agent.prepare_plan_handoff(&mut |_| {}, &mut resolve)?;
    test.agent.persist_compaction(
        "Implementation progress; consult saved plan.",
        0,
        1,
        &[],
        None,
    )?;
    compaction::apply_summary(
        &mut test.agent.messages,
        "Implementation progress; consult saved plan.",
        1,
    );
    let path = test.agent.plan_file_reference()?.unwrap();
    std::fs::remove_file(&path)?;
    test.agent.prepare_plan_handoff(&mut |_| {}, &mut resolve)?;
    assert_eq!(captures.borrow().len(), 1);
    assert_eq!(
        test.agent.plan_file_reference()?.as_deref(),
        Some(path.as_str())
    );
    assert!(std::fs::read_to_string(&path)?.contains("# Unique implementation plan"));

    let plan = "# Revised implementation plan";
    let ready = Message::assistant(plan.into(), vec![]);
    test.agent
        .persistent_mut()
        .session
        .append_plan_ready(plan, &ready)?;
    test.agent.messages.push(ready);
    test.agent
        .append_input_message(Message::user("Implement the revised plan"))?;
    test.agent.prepare_plan_handoff(&mut |_| {}, &mut resolve)?;
    assert_eq!(captures.borrow().len(), 2);
    assert!(test.agent.messages[0].content.contains(plan));
    assert!(
        !test.agent.messages[0]
            .content
            .contains("# Unique implementation plan")
    );
    assert!(test.agent.persistent_state().session.plan_has_handoff(2));
    assert_ne!(test.agent.plan_file_reference()?.unwrap(), path);
    Ok(())
}

#[test]
fn canceled_handoff_keeps_the_original_context() -> Result<(), Error> {
    struct CancelSummary(crate::cancellation::CancellationToken);
    impl Provider for CancelSummary {
        fn stream_once(
            &self,
            _: &Request<'_>,
            sink: &mut dyn FnMut(ProviderEvent),
        ) -> Result<(), Error> {
            sink(ProviderEvent::TextDelta("partial summary".into()));
            self.0.cancel();
            sink(ProviderEvent::Done);
            Ok(())
        }
    }
    let mut test = ready_handoff_fixture("handoff-cancel");
    test.agent
        .append_input_message(Message::user("Implement"))?;
    let before = serde_json::to_value(&test.agent.messages)?;
    let token = test.agent.cancellation.clone();
    let mut resolve = |_: &str, _: &Config| -> Result<(Box<dyn Provider>, String), Error> {
        Ok((Box::new(CancelSummary(token.clone())), "test".into()))
    };
    let result = crate::cancellation::scope(&token, || {
        test.agent.prepare_plan_handoff(&mut |_| {}, &mut resolve)
    });
    assert!(matches!(result, Err(Error::Interrupted)));
    assert_eq!(serde_json::to_value(&test.agent.messages)?, before);
    assert!(!test.agent.persistent_state().session.plan_has_handoff(1));
    token.clear();
    Ok(())
}
