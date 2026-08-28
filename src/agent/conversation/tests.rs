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
use crate::provider::{Event as ProviderEvent, Message, Provider, Request, Role, ToolCall};
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
    test.agent
        .subagents
        .as_ref()
        .expect("persistent conversations own a manager")
        .push_test_deferred(
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
    assert!(
        !test
            .agent
            .subagents
            .as_ref()
            .expect("manager survives the pump")
            .has_deferred()
    );
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
    let (_, replayed) = Session::open(&test.sessions_dir, test.agent.session.id())
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
fn undo_last_turn_drops_the_prompt_and_restores_files() {
    let mut test = TestAgent::new("undo-turn");
    let file = test.root.join("cwd").join("note.txt");
    std::fs::write(&file, "before").expect("write");
    test.agent
        .checkpoints
        .as_mut()
        .expect("persistent")
        .snapshot()
        .expect("snapshot");
    test.agent
        .checkpoints
        .as_mut()
        .expect("persistent")
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
