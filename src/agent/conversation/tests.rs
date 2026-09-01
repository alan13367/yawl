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

    conversation.sync_display_config(&active_config);

    assert_eq!(conversation.config().status_bar, active_config.status_bar);
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
