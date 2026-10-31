use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;

use super::TurnRequest;
use super::{Conversation, last_undoable_user_index};
use crate::agent::TurnEvent;
use crate::compaction;
use crate::config::{Config, ConfigChange};
use crate::error::Error;
use crate::provider::{
    CompactionOutput, Event as ProviderEvent, Message, MessageControl, Provider, Request, Role,
    TokenUsage, ToolCall,
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
    MalformedToolCall,
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
            ProviderStep::MalformedToolCall => {
                Err(Error::MalformedToolCall("unparseable tool call".into()))
            }
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

fn prompt(text: &str) -> TurnRequest {
    TurnRequest::Prompt(Some(text.to_string().into()))
}

struct TestAgent {
    root: PathBuf,
    sessions_dir: PathBuf,
    agent: Conversation,
}

impl TestAgent {
    fn new(name: &str) -> Self {
        let root = crate::test_support::unique_temp_dir(&format!("agent-{name}"));
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
            agent: Conversation::persistent(config, "test".into(), session, cwd),
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
    active_config.theme = crate::config::Theme::Light;
    active_config.selection_color = Some(crate::config::UiColor::WHITE);

    conversation.sync_display_config(&active_config);

    assert_eq!(conversation.config().status_bar, active_config.status_bar);
    assert!(!conversation.config().bell);
    assert_eq!(conversation.config().theme, crate::config::Theme::Light);
    assert_eq!(
        conversation.config().selection_color,
        active_config.selection_color
    );
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
        .map(|spec| spec.name.clone())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(
        names,
        ["read_file"],
        "preset children see only allowed tools"
    );
}

#[test]
fn a_main_preset_restricts_tools_and_adds_its_role_to_the_prompt() {
    let mut test = TestAgent::new("main-preset");
    let scout = crate::subagent::presets::bundled().remove(0);
    test.agent.set_print_mode();
    test.agent
        .apply_preset(scout.tools.clone(), scout.prompt.clone());

    let mut names = test
        .agent
        .scan_tools()
        .specs()
        .into_iter()
        .map(|spec| spec.name.clone())
        .collect::<Vec<_>>();
    names.sort();
    let mut expected = scout.tools.clone().expect("scout allowlist");
    // read_skill is only advertised when skills exist.
    expected.retain(|name| name != "read_skill");
    expected.sort();
    assert_eq!(names, expected, "no shell, writes, or define_tool");

    let steps = Rc::new(RefCell::new(VecDeque::from([ProviderStep::Output {
        text: "found it",
        tool_calls: Vec::new(),
        input_tokens: 1,
        output_tokens: 1,
    }])));
    let systems = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve_with_systems(steps, Rc::default(), Rc::clone(&systems));
    assert!(
        test.agent
            .run_with(prompt("where is the parser?"), &mut |_| {}, &mut resolve)
            .expect("turn runs")
    );
    let system = systems.borrow()[0].clone();
    assert!(system.contains("<agent_preset>"));
    assert!(system.contains("read-only research specialist"));
    assert!(system.contains("You may use only these tools"));
    assert!(system.contains("<non_interactive_run>"));
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
    let steps = Rc::new(RefCell::new(VecDeque::from([step(
        "delivered summary",
        Vec::new(),
    )])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));

    // Drive the pump's delivery turn with the injected resolver the same
    // way run drives it with the real one.
    let pumped = test
        .agent
        .pump_subagent_results_with(&mut |_| {}, 1, &mut resolve)
        .expect("pump should complete");

    assert!(pumped);
    assert!(!test.agent.subagent_manager().has_deferred());
    let messages = test.agent.messages();
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
fn mid_turn_reasoning_change_reaches_the_next_model_request() {
    let mut test = TestAgent::new("mid-turn-reasoning");
    let steps = Rc::new(RefCell::new(VecDeque::from([
        step(
            "",
            vec![call("call-1", "shell", r#"{"command":"printf ok"}"#)],
        ),
        step("done", Vec::new()),
    ])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let steers = test.agent.steers.clone();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = |_: &str, config: &Config| {
        let mut levels = seen.borrow_mut();
        levels.push(config.reasoning_effort.clone());
        if levels.len() == 1 {
            steers.set_reasoning_effort(Some("high".into()));
        }
        Ok::<(Box<dyn Provider>, String), Error>((
            Box::new(ScriptedProvider {
                steps: Rc::clone(&steps),
                requests: Rc::clone(&requests),
                systems: None,
            }),
            "test".into(),
        ))
    };

    assert!(
        test.agent
            .run_with(prompt("run it"), &mut |_| {}, &mut resolve)
            .expect("scripted turn should complete")
    );
    assert_eq!(seen.borrow().as_slice(), &[None, Some("high".into())]);
}

#[test]
fn conversation_transaction_persists_tool_loop_in_order() {
    let mut test = TestAgent::new("tool-loop");
    let steps = Rc::new(RefCell::new(VecDeque::from([
        step(
            "",
            vec![call(
                "call-1",
                "shell",
                r#"{"command":"printf tool-output"}"#,
            )],
        ),
        step("done", Vec::new()),
    ])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));

    let completed = test
        .agent
        .run_with(prompt("run it"), &mut |_| {}, &mut resolve)
        .expect("scripted turn should complete");

    assert!(completed);
    assert_eq!(
        test.agent
            .messages()
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>(),
        [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );
    assert_eq!(test.agent.messages()[2].content, "tool-output");
    assert_eq!(test.agent.messages()[3].content, "done");
    assert_eq!(
        requests.borrow().as_slice(),
        [
            vec![Role::User],
            vec![Role::User, Role::Assistant, Role::Tool]
        ]
    );
    let replayed = Session::open(&test.sessions_dir, test.agent.session_id())
        .expect("persisted session should replay")
        .messages()
        .to_vec();
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
        step(
            "",
            vec![ToolCall {
                id: "init-write".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({
                    "path": path.to_string_lossy(),
                    "content": content,
                })
                .to_string(),
            }],
        ),
        ProviderStep::Output {
            text: "Updated AGENTS.md.",
            tool_calls: Vec::new(),
            input_tokens: 12,
            output_tokens: 3,
        },
    ])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let systems = Rc::new(RefCell::new(Vec::new()));
    let mut resolve =
        scripted_resolve_with_systems(Rc::clone(&steps), Rc::clone(&requests), Rc::clone(&systems));

    let completed = test
        .agent
        .run_with(
            TurnRequest::Init("/init".to_string().into()),
            &mut |_| {},
            &mut resolve,
        )
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
    assert_eq!(test.agent.messages()[0].content, "/init");
    assert_eq!(systems.borrow().len(), 2);
    assert!(
        systems
            .borrow()
            .iter()
            .all(|system| system.contains("<init_task>"))
    );
    let replayed = Session::open(&test.sessions_dir, test.agent.session_id())
        .expect("init session should replay")
        .messages()
        .to_vec();
    assert_eq!(replayed[0].content, "/init");

    let report = test.agent.undo_last_turn().expect("undo init");
    assert!(report.restored_files);
    assert!(!agents.exists());
    assert!(test.agent.messages().is_empty());

    let steps = Rc::new(RefCell::new(VecDeque::from([ProviderStep::Output {
        text: "normal reply",
        tool_calls: Vec::new(),
        input_tokens: 5,
        output_tokens: 2,
    }])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let normal_systems = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve_with_systems(
        Rc::clone(&steps),
        Rc::clone(&requests),
        Rc::clone(&normal_systems),
    );
    test.agent
        .run_with(prompt("next"), &mut |_| {}, &mut resolve)
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
    let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));

    let result = test
        .agent
        .run_with(prompt("hello"), &mut |_| {}, &mut resolve);

    assert!(result.is_err());
    assert_eq!(test.agent.messages().len(), 1);
    assert_eq!(test.agent.messages()[0].role, Role::User);
}

#[test]
fn a_failed_turn_continues_from_saved_history_without_a_new_prompt() {
    crate::set_interrupted(false);
    let mut test = TestAgent::new("continue-after-failure");
    let steps = Rc::new(RefCell::new(VecDeque::from([
        ProviderStep::Fail,
        text_step("recovered"),
    ])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));

    assert!(
        !test.agent.can_continue(),
        "a fresh session has nothing to continue"
    );
    let failed = test
        .agent
        .run_with(prompt("hello"), &mut |_| {}, &mut resolve);
    assert!(failed.is_err());
    assert!(test.agent.can_continue());

    let resumed = test
        .agent
        .run_with(TurnRequest::Continue, &mut |_| {}, &mut resolve)
        .expect("continue should succeed");

    assert!(resumed);
    assert_eq!(
        requests.borrow().last().map(Vec::as_slice),
        Some([Role::User].as_slice()),
        "continuing must resend the original prompt without a duplicate"
    );
    let roles: Vec<Role> = test.agent.messages().iter().map(|m| m.role).collect();
    assert_eq!(roles, [Role::User, Role::Assistant]);
    assert!(
        !test.agent.can_continue(),
        "a finished turn is not continuable"
    );
    let error = test
        .agent
        .run_with(TurnRequest::Continue, &mut |_| {}, &mut resolve)
        .expect_err("nothing is left to continue");
    assert!(matches!(error, Error::Invalid(_)));
    assert_eq!(error.to_string(), "nothing to continue");
}

#[test]
fn a_session_left_mid_tool_call_can_continue_after_resume() {
    crate::set_interrupted(false);
    let mut test = TestAgent::new("continue-after-resume");
    let assistant = Message::assistant(String::new(), vec![call("call-1", "shell", "{}")]);
    *test.agent.session.messages_mut() = vec![Message::user("run it"), assistant];

    assert!(test.agent.can_continue());
    let steps = Rc::new(RefCell::new(VecDeque::from([text_step("done")])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));
    assert!(
        test.agent
            .run_with(TurnRequest::Continue, &mut |_| {}, &mut resolve)
            .expect("continue")
    );
    assert_eq!(
        requests.borrow()[0],
        [Role::User, Role::Assistant, Role::Tool],
        "the missing tool result is repaired before the request"
    );
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
    let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));

    let completed = test
        .agent
        .run_with(prompt("hello"), &mut |_| {}, &mut resolve)
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

    assert_eq!(test.agent.messages().len(), 11);
    assert_eq!(
        test.agent.messages()[0].provider_data[0]["type"],
        "compaction"
    );
    assert_eq!(
        test.agent.messages()[0].provider_data_model.as_deref(),
        Some("codex-test")
    );
    assert_eq!(test.agent.usage().requests, 2);
    let session = Session::open(&test.sessions_dir, test.agent.session_id())
        .expect("compacted session should replay");
    let replayed = session.messages().to_vec();
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
            .session
            .messages_mut()
            .push(Message::user(format!("history {index}")));
    }
    let steps = Rc::new(RefCell::new(VecDeque::from([
        ProviderStep::Output {
            text: "",
            tool_calls: vec![call("call-1", "shell", r#"{"command":"true"}"#)],
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
    let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));
    let mut warnings = Vec::new();

    let completed = test
        .agent
        .run_with(
            prompt("hello"),
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
    assert_eq!(test.agent.messages().len(), 16);
    assert_eq!(test.agent.messages()[14].role, Role::Tool);
    assert_eq!(test.agent.messages()[15].content, "done");
}

#[test]
fn interrupted_tool_batch_keeps_one_result_per_call() {
    let mut test = TestAgent::new("interrupted-tools");
    let registry = test.agent.scan_tools();
    let calls = [
        call("one", "shell", r#"{"command":"true"}"#),
        call("two", "shell", r#"{"command":"true"}"#),
    ];

    let aborted = test
        .agent
        .run_tools_while(&registry, &calls, &mut |_| {}, || true)
        .expect("synthetic tool results should persist");

    assert!(aborted);
    assert_eq!(test.agent.messages().len(), 2);
    assert!(test.agent.messages().iter().all(|message| {
        message.role == Role::Tool && message.is_error && message.content == "[interrupted by user]"
    }));
    assert_eq!(
        test.agent.messages()[0].tool_call_id.as_deref(),
        Some("one")
    );
    assert_eq!(
        test.agent.messages()[1].tool_call_id.as_deref(),
        Some("two")
    );
}

#[test]
fn last_undoable_user_skips_summaries_and_subagent_results() {
    let summary = Message::summary("old", Vec::new(), None);
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
    assert!(messages[0].starts_user_turn());
    assert!(!messages[2].starts_user_turn());
    assert!(!messages[6].starts_user_turn());
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

    let protected = last_undoable_user_index(test.agent.messages());
    let range = compaction::compaction_range(test.agent.messages(), protected);
    test.agent
        .session
        .append_compaction("compacted goal progress", range, Vec::new(), None)
        .expect("compaction should persist");

    assert_eq!(last_undoable_user_index(test.agent.messages()), Some(2));
    let report = test.agent.undo_last_turn().expect("undo should succeed");
    assert!(report.dropped > 0);
    assert_eq!(test.agent.messages().len(), 2);
    assert_eq!(test.agent.messages()[0].content, "earlier prompt");
    assert_eq!(test.agent.active_goal(), None);
}

#[test]
fn undo_last_turn_drops_the_prompt_and_restores_files() {
    let mut test = TestAgent::new("undo-turn");
    let file = test.root.join("cwd").join("note.txt");
    std::fs::write(&file, "before").expect("write");
    test.agent
        .services
        .checkpoints
        .as_mut()
        .unwrap()
        .snapshot()
        .expect("snapshot");
    test.agent
        .services
        .checkpoints
        .as_mut()
        .unwrap()
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
    assert!(test.agent.messages().is_empty());
    assert_eq!(std::fs::read_to_string(&file).expect("read"), "before");
}

#[test]
fn undo_does_not_persist_intent_when_checkpoint_store_cannot_be_read() {
    let mut test = TestAgent::new("undo-unreadable");
    test.agent
        .append_input_message(Message::user("keep this prompt"))
        .expect("user message");
    let checkpoint_dir = test
        .agent
        .config
        .home_dir
        .join("checkpoints")
        .join(test.agent.session_id());
    std::fs::create_dir_all(checkpoint_dir.join("stack.json")).expect("unreadable stack");
    reopen_test_agent(&mut test);

    let error = test
        .agent
        .undo_last_turn()
        .expect_err("undo should report the unreadable store");
    assert!(error.to_string().contains("cannot load undo checkpoints"));
    assert!(test.agent.session.pending_undo().is_none());
    assert_eq!(test.agent.messages()[0].content, "keep this prompt");
    assert!(checkpoint_dir.join("stack.json").is_dir());
}

#[test]
fn undo_without_a_user_turn_is_a_no_op() {
    let mut test = TestAgent::new("undo-empty");
    let report = test.agent.undo_last_turn().expect("undo");
    assert_eq!(report.dropped, 0);
    assert!(test.agent.messages().is_empty());
}

fn scripted_resolve(
    steps: Rc<RefCell<VecDeque<ProviderStep>>>,
    requests: Rc<RefCell<Vec<Vec<Role>>>>,
) -> impl FnMut(&str, &Config) -> Result<(Box<dyn Provider>, String), Error> {
    resolve_scripted(steps, requests, None)
}

/// Like [`scripted_resolve`], also recording each request's system prompt.
fn scripted_resolve_with_systems(
    steps: Rc<RefCell<VecDeque<ProviderStep>>>,
    requests: Rc<RefCell<Vec<Vec<Role>>>>,
    systems: Rc<RefCell<Vec<String>>>,
) -> impl FnMut(&str, &Config) -> Result<(Box<dyn Provider>, String), Error> {
    resolve_scripted(steps, requests, Some(systems))
}

fn resolve_scripted(
    steps: Rc<RefCell<VecDeque<ProviderStep>>>,
    requests: Rc<RefCell<Vec<Vec<Role>>>>,
    systems: Option<Rc<RefCell<Vec<String>>>>,
) -> impl FnMut(&str, &Config) -> Result<(Box<dyn Provider>, String), Error> {
    move |_: &str, _: &Config| {
        Ok::<(Box<dyn Provider>, String), Error>((
            Box::new(ScriptedProvider {
                steps: Rc::clone(&steps),
                requests: Rc::clone(&requests),
                systems: systems.clone(),
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
        step("still working", Vec::new()),
        step(
            "",
            vec![ToolCall {
                id: "goal-1".into(),
                name: crate::tools::GOAL_COMPLETE_TOOL_NAME.into(),
                arguments: r#"{"result":"shipped"}"#.into(),
            }],
        ),
    ])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));

    let completed = test
        .agent
        .run_with(TurnRequest::Goal, &mut |_| {}, &mut resolve)
        .expect("goal turn should complete");

    assert!(completed);
    assert_eq!(test.agent.active_goal(), None);
    assert_eq!(test.agent.latest_turn_result(), "shipped");
    assert!(
        test.agent
            .messages()
            .iter()
            .any(|message| message.is_hidden_control()),
        "goal continuation should be recorded in history"
    );
    assert_eq!(
        test.agent
            .messages()
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
    let steps = Rc::new(RefCell::new(VecDeque::from([step(
        "answer to the separate question",
        Vec::new(),
    )])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));

    let completed = test
        .agent
        .run_with(prompt("a separate question"), &mut |_| {}, &mut resolve)
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
            .messages()
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
        step(
            "",
            vec![
                ToolCall {
                    id: "goal-1".into(),
                    name: crate::tools::GOAL_COMPLETE_TOOL_NAME.into(),
                    arguments: r#"{"result":"done"}"#.into(),
                },
                call("shell-1", "shell", r#"{"command":"printf ok"}"#),
            ],
        ),
        step(
            "",
            vec![ToolCall {
                id: "goal-2".into(),
                name: crate::tools::GOAL_COMPLETE_TOOL_NAME.into(),
                arguments: r#"{"result":"done"}"#.into(),
            }],
        ),
    ])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));

    let completed = test
        .agent
        .run_with(TurnRequest::Goal, &mut |_| {}, &mut resolve)
        .expect("goal turn should complete");

    assert!(completed);
    assert_eq!(test.agent.active_goal(), None);
    let first_batch_results = test
        .agent
        .messages()
        .iter()
        .filter(|message| {
            message.role == Role::Tool
                && matches!(message.tool_call_id.as_deref(), Some("goal-1" | "shell-1"))
        })
        .map(|message| message.tool_call_id.as_deref().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(first_batch_results, ["goal-1", "shell-1"]);
    assert!(
        test.agent.messages().iter().any(|message| {
            message.role == Role::Tool
                && message.tool_name.as_deref() == Some(crate::tools::GOAL_COMPLETE_TOOL_NAME)
                && message.is_error
                && message.content.contains("only tool call")
        }),
        "mixed completion should be rejected before a later success"
    );
}

fn plan_question_batch(count: usize) -> String {
    serde_json::json!({
        "questions": (1..=count).map(|index| serde_json::json!({
            "id": format!("q{index}"),
            "question": format!("Choice {index}?"),
            "options": [
                {"label": "Recommended", "description": "use the default"},
                {"label": "Alternative", "description": "use another approach"}
            ],
            "recommended": 0
        })).collect::<Vec<_>>()
    })
    .to_string()
}

/// Answers `count` questions with their recommendations and reports whether
/// every one of them was shown without an auto-answer countdown.
fn answer_questions_without_countdown(
    broker: crate::tools::QuestionBroker,
    count: usize,
) -> std::thread::JoinHandle<bool> {
    std::thread::spawn(move || {
        let mut answered = 0;
        let mut waited = true;
        while answered < count {
            if let Some(snapshot) = broker.snapshot() {
                waited &= snapshot.remaining.is_none();
                broker
                    .answer(snapshot.request_id, snapshot.question.recommended)
                    .expect("answer question");
                answered += 1;
            } else {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        waited
    })
}

fn tool_step(id: &str, name: &str, arguments: impl Into<String>) -> ProviderStep {
    step(
        "",
        vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }],
    )
}

#[test]
fn planning_requires_one_to_three_questions_then_persists_the_completed_plan() {
    let mut test = TestAgent::new("plan-complete");
    test.agent.enable_interactive_questions();
    test.agent
        .start_plan("add planning mode".to_string().into())
        .expect("start plan");
    let plan = r##"{"plan":"# Plan\n\n1. Build it."}"##;
    let steps = Rc::new(RefCell::new(VecDeque::from([
        tool_step("plan-early", crate::tools::PLAN_COMPLETE_TOOL_NAME, plan),
        tool_step(
            "questions-invalid",
            crate::tools::USER_INPUT_TOOL_NAME,
            plan_question_batch(4),
        ),
        tool_step(
            "questions-1",
            crate::tools::USER_INPUT_TOOL_NAME,
            plan_question_batch(1),
        ),
        tool_step("plan-1", crate::tools::PLAN_COMPLETE_TOOL_NAME, plan),
    ])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));
    let responder = answer_questions_without_countdown(test.agent.question_broker(), 1);

    let completed = test
        .agent
        .run_with(TurnRequest::Plan, &mut |_| {}, &mut resolve)
        .expect("planning should complete");
    assert!(
        responder.join().expect("question responder"),
        "planning questions must not count down"
    );

    assert!(completed);
    assert!(test.agent.messages().iter().any(|message| {
        message.tool_call_id.as_deref() == Some("plan-early")
            && message.is_error
            && message.content == super::plan::PLAN_QUESTION_REQUIRED
    }));
    assert!(test.agent.messages().iter().any(|message| {
        message.tool_call_id.as_deref() == Some("questions-invalid")
            && message.is_error
            && message.content.contains("between 1 and 3")
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
            .run_with(
                TurnRequest::PlanImplement(Some("implement it".to_string().into())),
                &mut |_| {},
                &mut failed_resolve
            )
            .is_err()
    );
    assert_eq!(
        test.agent.active_plan(),
        Some("# Plan\n\n1. Build it."),
        "implementation errors must retain the active plan"
    );
}

/// A plan completed through one answered question, ready for revision.
fn ready_plan_fixture(name: &str) -> TestAgent {
    let mut test = TestAgent::new(name);
    test.agent.enable_interactive_questions();
    test.agent
        .start_plan("add planning mode".to_string().into())
        .expect("start plan");
    let steps = Rc::new(RefCell::new(VecDeque::from([
        tool_step(
            "questions-1",
            crate::tools::USER_INPUT_TOOL_NAME,
            plan_question_batch(1),
        ),
        tool_step(
            "plan-1",
            crate::tools::PLAN_COMPLETE_TOOL_NAME,
            r##"{"plan":"# Plan\n\n1. Build it."}"##,
        ),
    ])));
    let mut resolve = scripted_resolve(steps, Rc::new(RefCell::new(Vec::new())));
    let responder = answer_questions_without_countdown(test.agent.question_broker(), 1);
    assert!(
        test.agent
            .run_with(TurnRequest::Plan, &mut |_| {}, &mut resolve)
            .expect("planning should complete")
    );
    responder.join().expect("question responder");
    test
}

#[test]
fn revision_completes_without_questions_and_has_its_own_continuation() {
    let mut test = ready_plan_fixture("plan-revise-no-questions");
    let steps = Rc::new(RefCell::new(VecDeque::from([
        text_step("I will rename the step."),
        tool_step(
            "plan-2",
            crate::tools::PLAN_COMPLETE_TOOL_NAME,
            r##"{"plan":"# Plan\n\n1. Build it well."}"##,
        ),
    ])));
    let mut resolve = scripted_resolve(steps, Rc::new(RefCell::new(Vec::new())));

    let completed = test
        .agent
        .run_with(
            TurnRequest::PlanRevise("rename step 1".to_string().into()),
            &mut |_| {},
            &mut resolve,
        )
        .expect("revision should complete");

    assert!(completed);
    assert_eq!(
        test.agent.active_plan(),
        Some("# Plan\n\n1. Build it well.")
    );
    let continuations = test
        .agent
        .messages()
        .iter()
        .filter(|message| message.control == Some(MessageControl::PlanContinuation))
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>();
    assert_eq!(continuations, [super::plan::PLAN_REVISE_CONTINUATION]);
    assert!(
        !test
            .agent
            .messages()
            .iter()
            .any(|message| message.is_error && message.tool_call_id.as_deref() == Some("plan-2"))
    );
}

#[test]
fn plan_revision_requires_a_ready_plan() {
    let mut test = TestAgent::new("plan-revise-needs-plan");
    let steps = Rc::new(RefCell::new(VecDeque::new()));
    let mut resolve = scripted_resolve(steps, Rc::new(RefCell::new(Vec::new())));
    let result = test.agent.run_with(
        TurnRequest::PlanRevise("rename step 1".to_string().into()),
        &mut |_| {},
        &mut resolve,
    );
    assert!(matches!(result, Err(Error::Invalid(_))));
    assert!(test.agent.messages().is_empty());
}

#[test]
fn revision_questions_are_optional_and_wait_for_the_user() {
    let mut test = ready_plan_fixture("plan-revise-questions");
    let steps = Rc::new(RefCell::new(VecDeque::from([
        tool_step(
            "questions-2",
            crate::tools::USER_INPUT_TOOL_NAME,
            plan_question_batch(2),
        ),
        tool_step(
            "plan-2",
            crate::tools::PLAN_COMPLETE_TOOL_NAME,
            r##"{"plan":"# Plan\n\n1. Build it differently."}"##,
        ),
    ])));
    let mut resolve = scripted_resolve(steps, Rc::new(RefCell::new(Vec::new())));
    let responder = answer_questions_without_countdown(test.agent.question_broker(), 2);

    assert!(
        test.agent
            .run_with(
                TurnRequest::PlanRevise("use another approach".to_string().into()),
                &mut |_| {},
                &mut resolve
            )
            .expect("revision should complete")
    );
    assert!(
        responder.join().expect("question responder"),
        "revision questions must not count down"
    );
    assert_eq!(
        test.agent.active_plan(),
        Some("# Plan\n\n1. Build it differently.")
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
        step(
            "",
            vec![ToolCall {
                id: "questions-1".into(),
                name: crate::tools::USER_INPUT_TOOL_NAME.into(),
                arguments: question_arguments,
            }],
        ),
        step(
            "",
            vec![ToolCall {
                id: "plan-1".into(),
                name: crate::tools::PLAN_COMPLETE_TOOL_NAME.into(),
                arguments: r##"{"plan":"# Plan\n\n1. Build it."}"##.into(),
            }],
        ),
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
        .run_with(TurnRequest::Plan, &mut |_| {}, &mut resolve)
        .expect("planning should complete");
    responder.join().expect("question responder");

    assert!(completed);
    assert!(test.agent.plan_ready_this_turn());
    assert_eq!(test.agent.active_plan(), Some("# Plan\n\n1. Build it."));

    let reply_steps = Rc::new(RefCell::new(VecDeque::from([step(
        "This plan adds a planning mode before implementation starts.",
        Vec::new(),
    )])));
    let mut reply_resolve = scripted_resolve(reply_steps, Rc::new(RefCell::new(Vec::new())));

    let completed = test
        .agent
        .run_with(
            TurnRequest::PlanFollowUp("what is this plan about?".to_string().into()),
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
            .messages()
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
        step(
            "",
            vec![ToolCall {
                id: "questions-1".into(),
                name: crate::tools::USER_INPUT_TOOL_NAME.into(),
                arguments: question_arguments,
            }],
        ),
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
            .run_with(TurnRequest::Plan, &mut |_| {}, &mut first_resolve)
            .is_err()
    );
    responder.join().expect("question responder");
    assert!(
        test.agent
            .plan_state()
            .is_some_and(crate::session::PlanState::questions_asked)
    );

    let resumed_steps = Rc::new(RefCell::new(VecDeque::from([step(
        "",
        vec![ToolCall {
            id: "plan-1".into(),
            name: crate::tools::PLAN_COMPLETE_TOOL_NAME.into(),
            arguments: r##"{"plan":"# Plan\n\n1. Build it."}"##.into(),
        }],
    )])));
    let mut resumed_resolve = scripted_resolve(resumed_steps, Rc::new(RefCell::new(Vec::new())));
    assert!(
        test.agent
            .run_with(TurnRequest::Plan, &mut |_| {}, &mut resumed_resolve)
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
        .session
        .append_plan_ready("# Plan", ready)
        .expect("ready plan");
    let output_path = test.root.join("unrelated.txt");
    let steps = Rc::new(RefCell::new(VecDeque::from([
        step(
            "",
            vec![ToolCall {
                id: "classify-1".into(),
                name: crate::tools::PLAN_ACTION_TOOL_NAME.into(),
                arguments: r#"{"action":"unrelated"}"#.into(),
            }],
        ),
        step(
            "",
            vec![ToolCall {
                id: "write-1".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({
                    "path": output_path,
                    "content": "done"
                })
                .to_string(),
            }],
        ),
        step("Completed the unrelated request.", Vec::new()),
    ])));
    let mut resolve = scripted_resolve(steps, Rc::new(RefCell::new(Vec::new())));

    assert!(
        test.agent
            .run_with(
                TurnRequest::PlanFollowUp("write an unrelated file".to_string().into()),
                &mut |_| {},
                &mut resolve
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
        .session
        .append_plan_ready("# Plan", ready)
        .expect("ready plan");
    test.agent.subagent_manager().push_test_deferred(
        1,
        "worker",
        crate::subagent::RunOutcome::Completed,
        "child result",
    );
    let steps = Rc::new(RefCell::new(VecDeque::from([step(
        "",
        vec![ToolCall {
            id: "implemented-1".into(),
            name: crate::tools::PLAN_IMPLEMENTED_TOOL_NAME.into(),
            arguments: r#"{"result":"Implemented the plan."}"#.into(),
        }],
    )])));
    steps
        .borrow_mut()
        .push_front(step("Implement the agreed feature.", Vec::new()));
    let mut resolve = scripted_resolve(steps, Rc::new(RefCell::new(Vec::new())));

    assert!(
        test.agent
            .run_with(
                TurnRequest::PlanImplement(Some("implement it".to_string().into())),
                &mut |_| {},
                &mut resolve
            )
            .expect("implementation should complete")
    );
    assert!(test.agent.messages().iter().any(|message| {
        message
            .subagent_results
            .iter()
            .any(|result| result.content == "child result")
    }));
    assert_eq!(test.agent.active_plan(), None);
}

#[test]
fn steering_mid_response_runs_its_tool_calls_before_the_steer() {
    let mut test = TestAgent::new("steer-tools");
    let output_path = test.root.join("cwd/large.txt");
    let steps = Rc::new(RefCell::new(VecDeque::from([
        step(
            "writing",
            vec![
                call(
                    "write-1",
                    "write_file",
                    &serde_json::json!({ "path": output_path, "content": "large file" })
                        .to_string(),
                ),
                call("shell-1", "shell", r#"{"command":"printf tool-output"}"#),
            ],
        ),
        step("adjusted", Vec::new()),
    ])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));
    let steers = test.agent.steer_inbox();
    let mut steer_accepted = false;

    let completed = test
        .agent
        .run_with(
            prompt("start"),
            &mut |event| match event {
                TurnEvent::TextDelta("writing") => steers.push(crate::provider::TurnInput {
                    text: "use tabs instead".into(),
                    images: Vec::new(),
                }),
                TurnEvent::SteerAccepted { .. } => steer_accepted = true,
                _ => {}
            },
            &mut resolve,
        )
        .expect("steered turn should complete");

    assert!(completed);
    assert_eq!(
        std::fs::read_to_string(&output_path).expect("the streamed write should run"),
        "large file"
    );
    let messages = test.agent.messages();
    assert!(!messages.iter().any(Message::is_skipped_tool));
    let shell = messages
        .iter()
        .position(|message| message.role == Role::Tool && message.content.contains("tool-output"))
        .expect("the second call should run although the steer was pending");
    let steer = messages
        .iter()
        .position(|message| message.is_steering() && message.content == "use tabs instead")
        .expect("accepted steer should be recorded as a steering user message");
    assert!(steer > shell, "the steer follows the tool results");
    assert_eq!(requests.borrow().len(), 2);
    assert_eq!(requests.borrow()[1].last(), Some(&Role::User));
    assert!(steer_accepted, "steer acceptance should surface to the UI");
}

#[test]
fn steering_while_the_goal_completes_hands_the_steer_back_for_the_next_turn() {
    let mut test = TestAgent::new("steer-goal-complete");
    test.agent
        .start_goal("ship the feature".to_string().into())
        .expect("start goal");
    let steps = Rc::new(RefCell::new(VecDeque::from([step(
        "done",
        vec![call(
            "goal-1",
            crate::tools::GOAL_COMPLETE_TOOL_NAME,
            r#"{"result":"shipped"}"#,
        )],
    )])));
    let mut resolve = scripted_resolve(steps, Rc::new(RefCell::new(Vec::new())));
    let steers = test.agent.steer_inbox();

    let completed = test
        .agent
        .run_with(
            TurnRequest::Goal,
            &mut |event| {
                if matches!(event, TurnEvent::TextDelta(_)) {
                    steers.push("also update the docs".to_string().into());
                }
            },
            &mut resolve,
        )
        .expect("goal turn should complete");

    assert!(completed);
    assert_eq!(test.agent.active_goal(), None);
    assert_eq!(test.agent.latest_turn_result(), "shipped");
    let leftover = test.agent.take_unaccepted_steers();
    assert_eq!(leftover.len(), 1);
    assert_eq!(leftover[0].text, "also update the docs");
}

fn reopen_test_agent(test: &mut TestAgent) {
    let session = Session::open(&test.sessions_dir, test.agent.session_id()).unwrap();
    test.agent = Conversation::persistent(
        test.agent.config.clone(),
        "test".into(),
        session,
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
        test.agent.session.fail_append_after(0);
        assert!(test.agent.undo_last_turn().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second edit");
        assert!(test.agent.session.pending_undo().is_none());
        // The intent persists, files restore, then the history append fails.
        test.agent.session.fail_append_after(1);
        assert!(test.agent.undo_last_turn().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first edit");
        assert_eq!(test.agent.messages().len(), 2);
        assert_eq!(
            test.agent
                .services
                .checkpoints
                .as_ref()
                .unwrap()
                .last_index(),
            Some(1)
        );
        if reopen {
            reopen_test_agent(&mut test);
        }
        assert_eq!(test.agent.undo_last_turn().unwrap().dropped, 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first edit");
        assert_eq!(test.agent.messages().len(), 1);
        assert_eq!(
            test.agent
                .services
                .checkpoints
                .as_ref()
                .unwrap()
                .last_index(),
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
    test.agent.session.fail_append_after(2);
    assert!(test.agent.undo_last_turn().is_err());
    assert_eq!(test.agent.messages().len(), 1);
    reopen_test_agent(&mut test);
    let report = test.agent.undo_last_turn().unwrap();
    assert_eq!(report.dropped, 1);
    assert!(report.restored_files);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "first edit");
    assert_eq!(test.agent.messages().len(), 1);
    assert!(test.agent.session.pending_undo().is_none());
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
    assert_eq!(test.agent.messages().len(), 1);
    std::fs::remove_dir(&temporary).unwrap();
    reopen_test_agent(&mut test);
    test.agent.undo_last_turn().unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "first edit");
    assert_eq!(
        test.agent
            .services
            .checkpoints
            .as_ref()
            .unwrap()
            .last_index(),
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
    test.agent.session.fail_append_after(0);
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
    assert_eq!(test.agent.messages()[1].content, actual_output);
    assert!(test.agent.messages()[2].content.contains("not executed"));
    reopen_test_agent(&mut test);
    assert_eq!(test.agent.messages().len(), 3);
    assert_eq!(test.agent.messages()[1].content, actual_output);
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
    assert_eq!(test.agent.messages().len(), 4);
    assert_eq!(test.agent.messages()[1].content, "known output");
    assert_eq!(
        test.agent.messages()[2].tool_call_id.as_deref(),
        Some("unknown")
    );
    assert!(
        test.agent.messages()[2]
            .content
            .contains("may have executed")
    );
    assert_eq!(test.agent.messages()[3].content, "continue");
    reopen_test_agent(&mut test);
    assert_eq!(test.agent.messages().len(), 4);
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
    let session = Session::open(&test.sessions_dir, test.agent.session_id()).unwrap();
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
    let session = Session::open(&test.sessions_dir, &id).unwrap();
    let messages = session.messages().to_vec();
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
        cwd.clone(),
    );
    resumed.load_session(&id).unwrap();
    assert_eq!(resumed.model(), "other-model");
    assert_eq!(resumed.messages().len(), 1);
}

fn resume_with_reasoning(test: &TestAgent, id: &str, saved_default: Option<&str>) -> Conversation {
    let session = Session::open(&test.sessions_dir, id).unwrap();
    let mut config = test.agent.config().clone();
    config.reasoning_effort = saved_default.map(str::to_string);
    Conversation::persistent(
        config,
        test.agent.model().to_string(),
        session,
        test.root.join("cwd"),
    )
}

#[test]
fn resumed_session_keeps_a_reasoning_choice_over_the_saved_default() {
    let mut test = TestAgent::new("reasoning-resume");
    test.agent.config.reasoning_effort = Some("xhigh".into());
    test.agent.configured_reasoning_effort = Some("xhigh".into());
    test.agent
        .append_input_message(Message::user("start"))
        .unwrap();
    let id = test.agent.session_id().to_string();

    let resumed = resume_with_reasoning(&test, &id, Some("low"));
    assert_eq!(
        resumed.config().reasoning_effort.as_deref(),
        Some("low"),
        "a session with no reasoning choice uses the saved default"
    );
    drop(resumed);

    test.agent
        .set_reasoning_effort(Some("medium".into()))
        .unwrap();
    let resumed = resume_with_reasoning(&test, &id, Some("xhigh"));
    assert_eq!(
        resumed.config().reasoning_effort.as_deref(),
        Some("medium"),
        "resume keeps the effort chosen for the session"
    );
    drop(resumed);

    test.agent.set_reasoning_effort(None).unwrap();
    let resumed = resume_with_reasoning(&test, &id, Some("xhigh"));
    assert_eq!(
        resumed.config().reasoning_effort.as_deref(),
        None,
        "an explicit provider default stays cleared on resume"
    );
}

#[test]
fn config_reload_keeps_a_session_reasoning_choice_until_the_default_is_saved() {
    let mut test = TestAgent::new("reasoning-reload");
    test.agent.config.reasoning_effort = Some("xhigh".into());
    test.agent.configured_reasoning_effort = Some("xhigh".into());
    test.agent
        .append_input_message(Message::user("start"))
        .unwrap();
    test.agent
        .set_reasoning_effort(Some("medium".into()))
        .unwrap();

    test.agent
        .change_global_config(ConfigChange::Bell(false))
        .unwrap();
    assert_eq!(
        test.agent.config().reasoning_effort.as_deref(),
        Some("medium"),
        "reloading an unrelated setting keeps the session effort"
    );

    test.agent
        .change_global_config(ConfigChange::ReasoningEffort {
            stored: "high".into(),
            effective: Some("high".into()),
        })
        .unwrap();
    assert_eq!(
        test.agent.config().reasoning_effort.as_deref(),
        Some("high")
    );

    let id = test.agent.session_id().to_string();
    let session = Session::open(&test.sessions_dir, &id).unwrap();
    assert_eq!(session.recorded_reasoning(), None);
    let resumed = resume_with_reasoning(&test, &id, Some("xhigh"));
    assert_eq!(
        resumed.config().reasoning_effort.as_deref(),
        Some("xhigh"),
        "a cleared session choice uses the config opened with the session"
    );
}

fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
    }
}

/// A scripted reply with the token usage most tests do not care about.
fn step(text: &'static str, tool_calls: Vec<ToolCall>) -> ProviderStep {
    ProviderStep::Output {
        text,
        tool_calls,
        input_tokens: 10,
        output_tokens: 2,
    }
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
        let result = test.agent.run_with(
            prompt("continue"),
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
fn malformed_tool_calls_retry_with_a_transient_corrective_note() {
    for (failures, succeeds) in [(2, true), (3, false)] {
        let mut test = TestAgent::new("malformed-tool-call");
        let mut script = VecDeque::new();
        script.extend((0..failures).map(|_| ProviderStep::MalformedToolCall));
        script.push_back(text_step("done"));
        let steps = Rc::new(RefCell::new(script));
        let requests = Rc::new(RefCell::new(Vec::new()));
        let mut resolve = scripted_resolve(Rc::clone(&steps), Rc::clone(&requests));
        let mut warnings = 0;
        let result = test.agent.run_with(
            prompt("continue"),
            &mut |event| {
                if matches!(event, TurnEvent::Warning(_)) {
                    warnings += 1;
                }
            },
            &mut resolve,
        );

        assert_eq!(result.is_ok(), succeeds);
        assert_eq!(warnings, 2);
        let requests = requests.borrow();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0], vec![Role::User]);
        // One note, reused across retries.
        assert_eq!(requests[1], vec![Role::User, Role::User]);
        assert_eq!(requests[2], vec![Role::User, Role::User]);
        let roles: Vec<_> = test.agent.messages().iter().map(|m| m.role).collect();
        let expected = if succeeds {
            vec![Role::User, Role::Assistant]
        } else {
            vec![Role::User]
        };
        assert_eq!(roles, expected);
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
            .run_with(prompt("continue"), &mut |_| {}, &mut resolve)
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
    std::fs::write(&path, "x".repeat(56_000)).unwrap();
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
            .run_with(prompt("read the file"), &mut |_| {}, &mut resolve)
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
    test.agent.session.fail_append_after(0);
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
    assert_eq!(test.agent.messages().len(), 2);
    assert_eq!(test.agent.messages()[0].subagent_results.len(), 1);
}

/// System prompt, advertised tool names, and messages of each request.
type HandoffRequests = Rc<RefCell<Vec<(String, Vec<String>, Vec<Message>)>>>;

struct HandoffProvider {
    requests: HandoffRequests,
    classify: bool,
}

fn latest_plan_phase(messages: &[Message]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find(|message| message.control == Some(MessageControl::PlanPhase))
        .map(|message| message.content.as_str())
}

impl Provider for HandoffProvider {
    fn stream_once(
        &self,
        request: &Request<'_>,
        sink: &mut dyn FnMut(ProviderEvent),
    ) -> Result<(), Error> {
        self.requests.borrow_mut().push((
            request.system.into(),
            request.tools.iter().map(|tool| tool.name.clone()).collect(),
            request.messages.to_vec(),
        ));
        let follow_up = latest_plan_phase(request.messages)
            .is_some_and(|note| note.contains("phase=\"follow_up\""));
        if self.classify && follow_up {
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
    test.agent.session.append_message(investigation).unwrap();
    let plan = "# Unique implementation plan\n\nUpdate parsing and run checks.";
    let ready = Message::assistant(plan.into(), vec![]);
    test.agent.session.append_plan_ready(plan, ready).unwrap();
    test
}

#[test]
fn plan_handoff_reduces_requests_for_picker_and_classified_follow_up() -> Result<(), Error> {
    for classify in [false, true] {
        let mut test = ready_handoff_fixture("handoff-requests");
        let before = test
            .agent
            .messages()
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
            test.agent.run_with(
                TurnRequest::PlanFollowUp("Implement, preserving compatibility".to_string().into()),
                &mut |_| {},
                &mut resolve,
            )?;
        } else {
            test.agent
                .run_with(TurnRequest::PlanImplement(None), &mut |_| {}, &mut resolve)?;
        }
        let requests = captures.borrow();
        // No summarizer request: acceptance goes straight to implementation.
        assert_eq!(requests.len(), if classify { 2 } else { 1 });
        let (system, tools, messages) = requests.last().unwrap();
        for (other_system, other_tools, _) in requests.iter() {
            assert_eq!(
                other_system, system,
                "plan phases must share the system prompt"
            );
            assert_eq!(other_tools, tools, "plan phases must share the tool list");
        }
        assert!(!system.contains("<active_plan"));
        assert!(!system.contains("# Unique implementation plan"));
        let phase = latest_plan_phase(messages).expect("implementation phase note");
        assert!(phase.contains("phase=\"implementation\""));
        assert!(phase.contains("read the saved plan file"));
        let text = messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text.matches("# Unique implementation plan").count(), 1);
        assert!(text.contains("replaced by its completed plan"));
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
            // The follow-up request extends the planning history, so a
            // server's cached prefix of that history stays reusable.
            let (_, _, first) = &requests[0];
            assert!(first.len() > 3);
            assert!(
                first
                    .iter()
                    .any(|m| m.content.contains("large investigation output"))
            );
        }
        drop(requests);
        // Undo implementation retains the compact handoff and its ready plan.
        test.agent.undo_last_turn()?;
        assert!(test.agent.active_plan().is_some());
        assert!(test.agent.session.plan_has_handoff(1));
        let id = test.agent.session_id().to_string();
        test.agent.session = Session::open(&test.sessions_dir, &id)?;
        captures.borrow_mut().clear();
        test.agent
            .run_with(TurnRequest::PlanImplement(None), &mut |_| {}, &mut resolve)?;
        assert_eq!(captures.borrow().len(), 1);
        let requests = captures.borrow();
        let (_, _, resumed) = requests.last().unwrap();
        assert!(
            !resumed
                .iter()
                .any(|m| m.content.contains("large investigation output")),
            "resume after undo must keep the handoff"
        );
        drop(requests);
        let log = std::fs::read_to_string(test.sessions_dir.join(format!("{id}.jsonl")))?;
        assert!(
            log.contains("large investigation output"),
            "original history remains on disk"
        );
    }
    Ok(())
}

#[test]
fn plan_handoff_keeps_earlier_conversation_and_carries_requirements() -> Result<(), Error> {
    let mut test = TestAgent::new("handoff-scope");
    test.agent
        .append_input_message(Message::user("Earlier unrelated discussion"))?;
    let reply = Message::assistant("Earlier reply".into(), vec![]);
    test.agent.session.append_message(reply)?;
    test.agent
        .start_plan("Add a cache\nwith a TTL".to_string().into())?;
    let questions = serde_json::json!({"questions": [
        {"id": "store", "question": "Which store?", "recommended": 0, "options": [
            {"label": "Redis", "description": "shared"},
            {"label": "Memory", "description": "local"}]},
        {"id": "ttl", "question": "Which TTL?", "recommended": 0, "options": [
            {"label": "60s", "description": "short"},
            {"label": "1h", "description": "long"}]}
    ]});
    let ask = Message::assistant(
        String::new(),
        vec![ToolCall {
            id: "ask".into(),
            name: crate::tools::USER_INPUT_TOOL_NAME.into(),
            arguments: questions.to_string(),
        }],
    );
    let answers = serde_json::json!({"timed_out": false, "answers": [
        {"id": "store", "option_index": null, "label": "Other", "answer": "Redis cluster", "source": "user"},
        {"id": "ttl", "option_index": 0, "label": "60s", "answer": "60s", "source": "timeout"}
    ]});
    let planning = [
        ask,
        Message::tool_result(
            "ask",
            crate::tools::USER_INPUT_TOOL_NAME,
            answers.to_string(),
            false,
        ),
        Message::user("Also log cache misses").with_control(MessageControl::Steering),
        Message::assistant("large investigation output".into(), vec![]),
    ];
    for message in planning {
        test.agent.session.append_message(message)?;
    }
    let plan = "# Cache plan\n\nAdd the cache.";
    let ready = Message::assistant(plan.into(), vec![]);
    test.agent.session.append_plan_ready(plan, ready)?;
    test.agent.append_input_message(
        Message::user("Implement the active plan now.")
            .with_control(MessageControl::PlanImplementationStart),
    )?;

    let mut handoffs = Vec::new();
    test.agent.prepare_plan_handoff(&mut |event| match event {
        TurnEvent::PlanHandoff { replaced, .. } => handoffs.push(replaced),
        TurnEvent::Compacted { .. } => panic!("a plan handoff is not a compaction"),
        _ => {}
    })?;
    // Objective, question batch, answers, steer, investigation, and plan.
    assert_eq!(handoffs, [6]);
    let messages = test.agent.messages();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].content, "Earlier unrelated discussion");
    assert_eq!(messages[1].content, "Earlier reply");
    assert!(messages[2].is_plan_handoff());
    assert!(!messages[2].is_summary());
    let handoff = &messages[2].content;
    for expected in [
        "replaced by its completed plan",
        "Objective:\n> Add a cache\n> with a TTL",
        "- Q: Which store?\n  A: Redis cluster\n",
        "- Q: Which TTL?\n  A: 60s (recommended default; the user did not answer)",
        "- User: Also log cache misses",
        "Plan:\n\n# Cache plan",
    ] {
        assert!(
            handoff.contains(expected),
            "missing {expected:?} in {handoff}"
        );
    }
    assert!(!handoff.contains("large investigation output"));
    assert_eq!(last_undoable_user_index(messages), Some(3));

    let id = test.agent.session_id().to_string();
    let replayed = Session::open(&test.sessions_dir, &id)?.messages().to_vec();
    assert_eq!(
        serde_json::to_value(&replayed)?,
        serde_json::to_value(test.agent.messages())?,
        "replay rebuilds the same handoff"
    );
    Ok(())
}

#[test]
fn plan_turns_announce_phase_changes() -> Result<(), Error> {
    use crate::agent::PlanPhase;

    let mut test = ready_handoff_fixture("handoff-phases");
    let captures = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = |_: &str, _: &Config| -> Result<(Box<dyn Provider>, String), Error> {
        Ok((
            Box::new(HandoffProvider {
                requests: captures.clone(),
                classify: true,
            }),
            "test".into(),
        ))
    };
    let mut phases = Vec::new();
    test.agent.run_with(
        TurnRequest::PlanFollowUp("Implement it".to_string().into()),
        &mut |event| {
            if let TurnEvent::PlanPhase(phase) = event {
                phases.push(phase);
            }
        },
        &mut resolve,
    )?;
    assert_eq!(
        phases,
        [Some(PlanPhase::FollowUp), Some(PlanPhase::Implementing)]
    );
    Ok(())
}

#[test]
fn plan_implementation_stops_idle_replies_and_blocked_keeps_the_plan() -> Result<(), Error> {
    let mut test = ready_handoff_fixture("implement-idle");
    let requests = Rc::new(RefCell::new(Vec::new()));
    let steps = Rc::new(RefCell::new(VecDeque::new()));
    let mut resolve = |_: &str, _: &Config| -> Result<(Box<dyn Provider>, String), Error> {
        Ok((
            Box::new(PhaseProvider {
                requests: requests.clone(),
                steps: steps.clone(),
            }),
            "test".into(),
        ))
    };
    let mut warnings = Vec::new();
    // The provider only ever replies with text.
    assert!(test.agent.run_with(
        TurnRequest::PlanImplement(None),
        &mut |event| {
            if let TurnEvent::Warning(text) = event {
                warnings.push(text);
            }
        },
        &mut resolve
    )?);
    assert_eq!(requests.borrow().len(), 3);
    assert!(
        warnings
            .iter()
            .any(|text| text.contains("plan stays active"))
    );
    assert!(test.agent.active_plan().is_some());

    let finish = |arguments: &str| {
        Some(ToolCall {
            id: "finish".into(),
            name: crate::tools::PLAN_IMPLEMENTED_TOOL_NAME.into(),
            arguments: arguments.into(),
        })
    };
    steps.borrow_mut().push_back(finish(
        r#"{"result":"Need the API key","status":"blocked"}"#,
    ));
    test.agent.run_with(
        TurnRequest::PlanImplement(Some("Keep going".to_string().into())),
        &mut |_| {},
        &mut resolve,
    )?;
    assert!(test.agent.active_plan().is_some(), "blocked keeps the plan");
    let last = test.agent.messages().last().expect("blocked reply");
    assert_eq!(last.content, "Need the API key");
    assert!(last.tool_calls.is_empty());

    // A reply to the blocked turn arrives as an ordinary follow-up; its note
    // says implementation already started, so the answer resumes it.
    requests.borrow_mut().clear();
    steps.borrow_mut().extend([
        Some(ToolCall {
            id: "classify".into(),
            name: crate::tools::PLAN_ACTION_TOOL_NAME.into(),
            arguments: r#"{"action":"implement"}"#.into(),
        }),
        finish(r#"{"result":"Done."}"#),
    ]);
    test.agent.run_with(
        TurnRequest::PlanFollowUp("Here is the key".to_string().into()),
        &mut |_| {},
        &mut resolve,
    )?;
    let follow_up_note = requests.borrow()[0]
        .2
        .iter()
        .rev()
        .find(|message| message.control == Some(MessageControl::PlanPhase))
        .map(|message| message.content.clone())
        .expect("follow-up phase note");
    assert!(follow_up_note.contains("already started and stopped before it finished"));
    assert!(test.agent.active_plan().is_none());
    Ok(())
}

#[test]
fn follow_up_before_implementation_uses_the_plain_classifier_note() -> Result<(), Error> {
    let mut test = ready_handoff_fixture("follow-up-before-implementation");
    let requests = Rc::new(RefCell::new(Vec::new()));
    let steps = Rc::new(RefCell::new(VecDeque::new()));
    let mut resolve = |_: &str, _: &Config| -> Result<(Box<dyn Provider>, String), Error> {
        Ok((
            Box::new(PhaseProvider {
                requests: requests.clone(),
                steps: steps.clone(),
            }),
            "test".into(),
        ))
    };
    test.agent.run_with(
        TurnRequest::PlanFollowUp("What does step two mean?".to_string().into()),
        &mut |_| {},
        &mut resolve,
    )?;
    let note = requests.borrow()[0]
        .2
        .iter()
        .rev()
        .find(|message| message.control == Some(MessageControl::PlanPhase))
        .map(|message| message.content.clone())
        .expect("follow-up phase note");
    assert!(note.contains("phase=\"follow_up\""));
    assert!(!note.contains("already started"));
    Ok(())
}

#[test]
fn failed_plan_handoff_preserves_history_and_retries() -> Result<(), Error> {
    let mut test = ready_handoff_fixture("handoff-failure");
    test.agent
        .append_input_message(Message::user("Implement with care"))?;
    let before = serde_json::to_value(test.agent.messages())?;
    test.agent.session.fail_append_after(0);
    assert!(test.agent.prepare_plan_handoff(&mut |_| {}).is_err());
    assert_eq!(serde_json::to_value(test.agent.messages())?, before);
    assert!(!test.agent.session.plan_has_handoff(1));
    assert!(test.agent.active_plan().is_some());
    test.agent.prepare_plan_handoff(&mut |_| {})?;
    assert_eq!(test.agent.messages().len(), 2);
    assert!(test.agent.session.plan_has_handoff(1));
    Ok(())
}

#[test]
fn plan_handoff_survives_compaction_and_new_revision_gets_a_new_handoff() -> Result<(), Error> {
    let mut test = ready_handoff_fixture("handoff-revision");
    test.agent
        .append_input_message(Message::user("Implement"))?;
    test.agent.prepare_plan_handoff(&mut |_| {})?;
    test.agent.session.append_compaction(
        "Implementation progress; consult saved plan.",
        0..1,
        Vec::new(),
        None,
    )?;
    let path = test.agent.plan_file_reference()?.unwrap();
    std::fs::remove_file(&path)?;
    let compacted = serde_json::to_value(test.agent.messages())?;
    test.agent.prepare_plan_handoff(&mut |_| {})?;
    assert_eq!(
        serde_json::to_value(test.agent.messages())?,
        compacted,
        "an existing handoff must not replace later history"
    );
    assert_eq!(
        test.agent.plan_file_reference()?.as_deref(),
        Some(path.as_str())
    );
    assert!(std::fs::read_to_string(&path)?.contains("# Unique implementation plan"));

    let plan = "# Revised implementation plan";
    let ready = Message::assistant(plan.into(), vec![]);
    test.agent.session.append_plan_ready(plan, ready)?;
    test.agent
        .append_input_message(Message::user("Implement the revised plan"))?;
    test.agent.prepare_plan_handoff(&mut |_| {})?;
    assert!(test.agent.messages()[0].content.contains(plan));
    assert!(
        !test.agent.messages()[0]
            .content
            .contains("# Unique implementation plan")
    );
    assert!(test.agent.session.plan_has_handoff(2));
    assert_ne!(test.agent.plan_file_reference()?.unwrap(), path);
    Ok(())
}

/// Replays scripted tool calls (or a text reply for `None`) while recording
/// each request's system prompt, tools, and messages.
struct PhaseProvider {
    requests: HandoffRequests,
    steps: Rc<RefCell<VecDeque<Option<ToolCall>>>>,
}

impl Provider for PhaseProvider {
    fn stream_once(
        &self,
        request: &Request<'_>,
        sink: &mut dyn FnMut(ProviderEvent),
    ) -> Result<(), Error> {
        self.requests.borrow_mut().push((
            request.system.into(),
            request.tools.iter().map(|tool| tool.name.clone()).collect(),
            request.messages.to_vec(),
        ));
        match self.steps.borrow_mut().pop_front().flatten() {
            Some(call) => sink(ProviderEvent::ToolCall(call)),
            None => sink(ProviderEvent::TextDelta("ok".into())),
        }
        sink(ProviderEvent::Done);
        Ok(())
    }
}

#[test]
fn plan_phases_keep_the_prompt_prefix_and_gate_tools_in_history_notes() -> Result<(), Error> {
    let mut test = TestAgent::new("plan-phase-prefix");
    test.agent.start_plan("Add a parser".to_string().into())?;
    test.agent.session.append_plan_questions_asked()?;
    let blocked = test.root.join("blocked.txt");
    let call = |id: &str, name: &str, arguments: String| {
        Some(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        })
    };
    let steps = Rc::new(RefCell::new(VecDeque::from([
        call(
            "write",
            "write_file",
            serde_json::json!({"path": blocked, "content": "no"}).to_string(),
        ),
        call(
            "complete",
            crate::tools::PLAN_COMPLETE_TOOL_NAME,
            r##"{"plan":"# Parser plan"}"##.into(),
        ),
        call(
            "done",
            crate::tools::PLAN_IMPLEMENTED_TOOL_NAME,
            r#"{"result":"Parser added."}"#.into(),
        ),
        None,
    ])));
    let captures: HandoffRequests = Rc::new(RefCell::new(Vec::new()));
    let mut resolve = |_: &str, _: &Config| -> Result<(Box<dyn Provider>, String), Error> {
        Ok((
            Box::new(PhaseProvider {
                requests: captures.clone(),
                steps: steps.clone(),
            }),
            "test".into(),
        ))
    };

    assert!(
        test.agent
            .run_with(TurnRequest::Plan, &mut |_| {}, &mut resolve)?
    );
    assert!(!blocked.exists(), "planning must not run write_file");
    let rejected = test
        .agent
        .messages()
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("write"))
        .expect("write result");
    assert!(rejected.is_error);
    assert!(rejected.content.contains("unavailable while planning"));

    assert!(
        test.agent
            .run_with(TurnRequest::PlanImplement(None), &mut |_| {}, &mut resolve)?
    );
    assert_eq!(test.agent.active_plan(), None);
    assert!(
        test.agent
            .run_with(prompt("thanks"), &mut |_| {}, &mut resolve)?
    );

    let requests = captures.borrow();
    assert_eq!(requests.len(), 4);
    // Draft planning and implementation send the same prefix; only the
    // hidden phase notes in history differ.
    for (system, tools, _) in &requests[1..3] {
        assert_eq!(system, &requests[0].0);
        assert_eq!(tools, &requests[0].1);
    }
    assert!(requests[0].1.iter().any(|name| name == "write_file"));
    assert!(!requests[0].0.contains("<active_plan"));
    let phases = requests
        .iter()
        .map(|(_, _, messages)| latest_plan_phase(messages).unwrap_or_default())
        .collect::<Vec<_>>();
    assert!(phases[0].contains("phase=\"planning\""));
    assert!(phases[0].contains("Add a parser"));
    assert!(phases[2].contains("phase=\"implementation\""));
    assert_eq!(phases[3], super::plan::PLAN_MODE_ENDED);
    // After completion the plan tools leave the list again.
    assert!(
        !requests[3]
            .1
            .iter()
            .any(|name| name == crate::tools::PLAN_COMPLETE_TOOL_NAME)
    );
    // Phase notes are hidden and never become undo points.
    assert!(
        test.agent
            .messages()
            .iter()
            .filter(|m| m.control == Some(MessageControl::PlanPhase))
            .all(|m| m.is_hidden_control() && !m.starts_user_turn())
    );
    Ok(())
}

#[test]
fn defined_tools_work_on_the_next_step_reach_the_prompt_and_undo_cleanly() {
    let mut test = TestAgent::new("define-tool");
    let home_tools = test.root.join("home/tools");
    std::fs::create_dir_all(&home_tools).expect("home tools");
    std::fs::write(home_tools.join("broken.json"), "{").expect("broken manifest");
    let manifest = test.root.join("project/tools/greet.json");
    let steps = Rc::new(RefCell::new(VecDeque::from([
        step(
            "",
            vec![call(
                "define",
                crate::tools::DEFINE_TOOL_NAME,
                &serde_json::json!({
                    "name": "greet",
                    "description": "Greet someone by name",
                    "params": {"who": {"type": "string"}},
                    "command": "printf 'hello %s' {{who}}",
                    "scope": "project"
                })
                .to_string(),
            )],
        ),
        step("", vec![call("use", "greet", r#"{"who":"Ada"}"#)]),
        step("Saved and tried greet.", Vec::new()),
        step("Still here.", Vec::new()),
    ])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let systems = Rc::new(RefCell::new(Vec::new()));
    let mut resolve =
        scripted_resolve_with_systems(Rc::clone(&steps), Rc::clone(&requests), Rc::clone(&systems));
    let mut warnings = Vec::new();
    let mut sink = |event: TurnEvent<'_>| {
        if let TurnEvent::Warning(text) = event {
            warnings.push(text);
        }
    };

    test.agent
        .run_with(prompt("make me a greeting tool"), &mut sink, &mut resolve)
        .expect("defining turn should complete");
    test.agent
        .run_with(prompt("thanks"), &mut sink, &mut resolve)
        .expect("follow-up turn should complete");

    let results = test
        .agent
        .messages()
        .iter()
        .filter(|message| message.role == Role::Tool)
        .map(|message| (message.is_error, message.content.clone()))
        .collect::<Vec<_>>();
    assert!(!results[0].0, "{}", results[0].1);
    assert!(results[0].1.starts_with("Created project tool `greet`"));
    assert_eq!(results[1], (false, "hello Ada".to_string()));
    let systems = systems.borrow();
    assert!(systems[0].contains("Personalize Yawl with define_tool"));
    assert!(!systems[0].contains("Custom tools saved for this user"));
    assert!(systems[1].contains("Custom tools saved for this user: greet."));
    let broken = warnings
        .iter()
        .filter(|warning| warning.contains("broken.json"))
        .count();
    assert_eq!(broken, 1, "{warnings:?}");

    assert!(manifest.is_file());
    test.agent.undo_last_turn().expect("undo follow-up");
    let report = test.agent.undo_last_turn().expect("undo definition");
    assert!(report.restored_files);
    assert!(!manifest.exists(), "undo should remove the defined tool");
}

#[test]
fn undo_after_late_subagent_results_restores_the_ready_plan() {
    let mut test = ready_plan_fixture("undo-late-subagent-results");
    test.agent.checkpoint_snapshot().unwrap().unwrap();
    test.agent
        .append_input_message(
            Message::user("Implement the active plan now.")
                .with_control(MessageControl::PlanImplementationStart),
        )
        .unwrap();
    // Implementation finishes while a background subagent still runs; its
    // result arrives afterwards.
    test.agent
        .session
        .append_plan_implemented(Message::assistant("implemented".into(), vec![]))
        .unwrap();
    test.agent
        .append_input_message(Message::subagent_results(vec![
            crate::provider::SubagentResult {
                id: "sa-1".into(),
                name: "explorer".into(),
                status: "completed".into(),
                run_number: 1,
                content: "found it".into(),
            },
        ]))
        .unwrap();

    let report = test.agent.undo_last_turn().unwrap();
    assert_eq!(report.dropped, 3);
    assert!(
        matches!(
            test.agent.plan_state(),
            Some(crate::session::PlanState::Ready { .. })
        ),
        "undo lost the ready plan: {:?}",
        test.agent.plan_state()
    );
}

/// An undo left pending (crash between its two log writes) whose snapshots
/// are gone must neither block leaving the session nor wedge it.
#[test]
fn pending_undo_with_missing_checkpoints_degrades_to_a_warning() {
    let mut test = TestAgent::new("undo-missing-checkpoints");
    let path = two_checkpoint_turns(&mut test);
    test.agent.session.fail_append_after(1);
    assert!(test.agent.undo_last_turn().is_err());
    let home = test.agent.config.home_dir.clone();
    std::fs::remove_dir_all(home.join("checkpoints")).unwrap();
    reopen_test_agent(&mut test);

    let report = test.agent.undo_last_turn().unwrap();
    assert_eq!(report.dropped, 1);
    assert!(!report.restored_files);
    assert!(report.warning.unwrap().contains("missing"));
    // The interrupted attempt already restored the file before its history
    // write failed.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "first edit");
    assert_eq!(test.agent.messages().len(), 1);
    assert!(test.agent.session.pending_undo().is_none());
}

#[test]
fn a_session_that_cannot_recover_does_not_block_a_new_one() {
    let mut test = TestAgent::new("undo-blocked-reset");
    two_checkpoint_turns(&mut test);
    test.agent.session.fail_append_after(1);
    assert!(test.agent.undo_last_turn().is_err());
    // A corrupt checkpoint store keeps the old session from recovering.
    let store = test
        .agent
        .config
        .home_dir
        .join("checkpoints")
        .join(test.agent.session_id());
    std::fs::write(store.join("stack.json"), "{not json").unwrap();
    reopen_test_agent(&mut test);
    assert!(test.agent.undo_last_turn().is_err());

    test.agent
        .reset()
        .expect("/new must not depend on the old session");
    assert!(test.agent.messages().is_empty());
    assert!(test.agent.session.pending_undo().is_none());
}
