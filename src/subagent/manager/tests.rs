use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::agent::Conversation;
use crate::config::{Config, ProviderConfig};
use crate::subagent::types::{
    MAX_QUEUE_MESSAGES, MAX_TRACKED_SUBAGENTS, RunOrigin, RunOutcome, SubagentId, SubagentSnapshot,
    SubagentStatus, SubagentTranscriptItem,
};

use super::capacity::prune_settled;
use super::execution::apply_turn_event;
use super::format::salvage_result;
use super::validation::{resolve_model, validate_id_list, wait_timeout};
use super::*;

fn config() -> Config {
    Config {
        model: Some("parent".into()),
        subagents: true,
        auto_compact: false,
        ..Config::test_default()
    }
}

fn provider_config(base_url: String) -> Config {
    let mut config = config();
    config.model = Some("local:model".into());
    config.providers.insert(
        "local".into(),
        ProviderConfig {
            base_url,
            api: "openai-completions".into(),
            api_key: None,
            auth_header: Some(false),
            headers: HashMap::new(),
            models: Vec::new(),
            compat: crate::config::OpenAiCompatibility::default(),
        },
    );
    config
}

fn test_entry(status: SubagentStatus) -> Entry {
    let conversation = Conversation::memory(config(), "parent".into(), "session-sa-1".into());
    let cancellation = conversation.cancellation_token();
    let mut snapshot = SubagentSnapshot::new(
        SubagentId::new(1),
        "agent".into(),
        "default".into(),
        "task".into(),
        "parent".into(),
        100,
    );
    snapshot.status = status;
    Entry {
        snapshot,
        cancellation,
        work: VecDeque::new(),
        next_run_number: 2,
        thread_id: None,
        handle: None,
        wait_interest: 0,
        pending_delivery: Vec::new(),
        suppress_delivery: false,
        steers: crate::agent::SteerInbox::default(),
    }
}

#[test]
fn usage_is_counted_before_an_active_subagent_settles() {
    let manager = SubagentManager::new("session".into(), 1);
    let id = SubagentId::new(1);
    {
        let mut state = manager.lock();
        state.entries.push(test_entry(SubagentStatus::Running));
        apply_turn_event(
            &mut state,
            &id,
            crate::agent::TurnEvent::Usage {
                context_tokens: 120,
                context_window: 1_000,
                request_usage: crate::provider::TokenUsage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cached_input_tokens: 75,
                    cache_write_input_tokens: 0,
                    cache_details_reported: true,
                },
                session_usage: crate::provider::UsageSummary::default(),
            },
        );
        assert_eq!(state.entries[0].snapshot.status, SubagentStatus::Running);
    }

    let usage = manager.total_child_usage();
    assert_eq!(usage.requests, 1);
    assert_eq!(usage.tokens.total_tokens(), 120);
    assert_eq!(usage.cache_hit_percent(), 75);
    manager.shutdown_and_discard();
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];
    let mut wanted = None;
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Ok(String::new());
        }
        request.extend_from_slice(&buffer[..read]);
        if wanted.is_none()
            && let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
        {
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            wanted = Some(header_end + 4 + content_length);
        }
        if wanted.is_some_and(|wanted| request.len() >= wanted) {
            return Ok(String::from_utf8_lossy(&request).into_owned());
        }
    }
}

fn write_response(stream: &mut TcpStream, text: &str) -> std::io::Result<()> {
    let body = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}},\"finish_reason\":null}}]}}\n\ndata: [DONE]\n\n"
    );
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    stream.flush()
}

fn write_multiline_response(stream: &mut TcpStream, text: &str) -> std::io::Result<()> {
    let event = serde_json::json!({
        "choices": [{"delta": {"content": text}, "finish_reason": null}]
    });
    let body = format!("data: {event}\n\ndata: [DONE]\n\n");
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    stream.flush()
}

/// Streams `text` followed by an unknown-tool call and a usage event, so
/// the child keeps issuing requests while burning budget.
fn write_tool_call_response(
    stream: &mut TcpStream,
    text: &str,
    call_id: &str,
) -> std::io::Result<()> {
    let mut body = String::new();
    if !text.is_empty() {
        let event = serde_json::json!({"choices": [{"delta": {"content": text}}]});
        body.push_str(&format!("data: {event}\n\n"));
    }
    let call = serde_json::json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": call_id,
            "type": "function",
            "function": {"name": "noop_probe", "arguments": "{}"}
        }]}}]
    });
    body.push_str(&format!("data: {call}\n\n"));
    let usage = serde_json::json!({
        "choices": [],
        "usage": {"prompt_tokens": 10, "completion_tokens": 2}
    });
    body.push_str(&format!("data: {usage}\n\n"));
    body.push_str("data: [DONE]\n\n");
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    stream.flush()
}

#[test]
fn wait_reports_the_complete_final_result() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
    let base_url = format!(
        "http://{}/v1",
        listener.local_addr().expect("test provider address")
    );
    let answer = (1..=30)
        .map(|line| format!("summary line {line:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("subagent provider connection");
        read_request(&mut stream).expect("subagent provider request");
        write_multiline_response(&mut stream, &answer).expect("subagent provider response");
    });
    let manager = SubagentManager::new("session".into(), 1);
    let id = manager
        .spawn(
            provider_config(base_url),
            "local:model",
            Some("scanner"),
            "scan the library",
            None,
        )
        .expect("subagent spawn");
    let waited = manager
        .wait(&[id.to_string()], Some(5))
        .expect("subagent should settle");

    assert!(waited.contains("summary line 01"));
    assert!(
        waited.contains("summary line 30"),
        "wait output should carry the complete answer; got:\n{waited}"
    );
    assert!(!manager.has_deferred());
    server.join().expect("provider server should exit");
    manager.shutdown_and_discard();
}

#[test]
fn large_reports_are_bounded_in_deliveries_and_readable_after_shutdown() {
    let root = std::env::temp_dir().join(format!("yawl-report-delivery-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let mut config = provider_config(format!(
        "http://{}/v1",
        listener.local_addr().expect("address")
    ));
    config.home_dir = root.clone();
    let read_config = config.clone();
    let report = format!(
        "Summary: traced cancellation.\n{}\nFINAL EVIDENCE",
        "detailed evidence é\n".repeat(500)
    );
    let expected = report.clone();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("connection");
        read_request(&mut stream).expect("request");
        write_multiline_response(&mut stream, &report).expect("response");
        let (mut stream, _) = listener.accept().expect("follow-up connection");
        read_request(&mut stream).expect("follow-up request");
        write_response(&mut stream, "short follow-up").expect("follow-up response");
    });
    let manager = SubagentManager::new("reports".into(), 1);
    let id = manager
        .spawn(config, "local:model", None, "investigate", None)
        .expect("spawn");
    assert!(manager.wait_all(5));
    let deliveries = manager.drain_deferred();
    assert_eq!(deliveries.len(), 1);
    assert!(deliveries[0].result.len() < 2048);
    assert!(
        deliveries[0]
            .result
            .contains("Summary: traced cancellation")
    );
    assert!(!deliveries[0].result.contains("FINAL EVIDENCE"));
    manager.restore_deferred(deliveries.clone());
    let waited = manager.wait(&[id.to_string()], Some(5)).expect("wait");
    let listed = manager.list(Some(id.as_str())).expect("list");
    let path = std::fs::read_dir(root.join("artifacts/subagents"))
        .expect("reports")
        .next()
        .expect("one report")
        .expect("entry")
        .path();
    for result in [&waited, &listed, &deliveries[0].result] {
        assert!(result.contains(&path.display().to_string()));
        assert!(!result.contains("FINAL EVIDENCE"));
    }
    manager
        .send(id.as_str(), "follow up", RunOrigin::Model)
        .expect("restart");
    let follow_up = manager.wait(&[id.to_string()], Some(5)).expect("follow-up");
    assert!(follow_up.contains("short follow-up"));
    server.join().expect("server");
    manager.shutdown_and_discard();

    // Retrieval uses only the persisted path, with no live child or manager.
    let registry =
        crate::tools::Registry::scan(&read_config, &mut crate::tools::CatalogCache::default());
    let mut offset = 0;
    let mut restored = String::new();
    loop {
        let outcome = registry.execute(
            "read_file",
            &serde_json::json!({"path": path, "offset": offset, "limit": 1024}).to_string(),
            "resumed",
        );
        assert!(!outcome.is_error, "{}", outcome.content);
        let (header, content) = outcome
            .content
            .split_once("\n\n")
            .expect("page header and text");
        restored.push_str(content);
        if let Some((_, next)) = header.rsplit_once("next_offset=") {
            let next: u64 = next.parse().expect("next offset");
            assert!(next > offset);
            offset = next;
        } else {
            assert!(header.ends_with("EOF"));
            break;
        }
    }
    assert_eq!(restored, expected);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn wait_reports_earlier_run_results_after_a_follow_up() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
    let base_url = format!(
        "http://{}/v1",
        listener.local_addr().expect("test provider address")
    );
    let server = std::thread::spawn(move || {
        for response in ["first full result", "second full result"] {
            let (mut stream, _) = listener.accept().expect("subagent provider connection");
            read_request(&mut stream).expect("subagent provider request");
            write_response(&mut stream, response).expect("subagent provider response");
        }
    });
    let manager = SubagentManager::new("session".into(), 1);
    let id = manager
        .spawn(
            provider_config(base_url),
            "local:model",
            Some("reused"),
            "first task",
            None,
        )
        .expect("initial subagent spawn");
    manager
        .send(id.as_str(), "follow-up", RunOrigin::Model)
        .expect("follow-up should queue or restart");
    let waited = manager
        .wait(&[id.to_string()], Some(5))
        .expect("both subagent runs should settle");

    assert!(
        waited.contains("first full result"),
        "earlier run results must survive into the wait; got:\n{waited}"
    );
    assert!(waited.contains("second full result"));
    assert!(!manager.has_deferred());
    server.join().expect("provider server should exit");
    manager.shutdown_and_discard();
}

#[test]
fn model_precedence_prefers_config_then_parent() {
    let mut config = provider_config("http://127.0.0.1:9/v1".into());
    config.subagent_model = "local:configured".into();
    assert_eq!(
        resolve_model(&config, "local:parent", None).expect("configured model"),
        "local:configured"
    );
    config.subagent_model = "inherit".into();
    assert_eq!(
        resolve_model(&config, "local:parent", None).expect("inherited model"),
        "local:parent"
    );
}

#[test]
fn unresolvable_configured_models_are_rejected_at_spawn_time() {
    let mut config = config();
    config.providers.insert(
        "broken".into(),
        ProviderConfig {
            base_url: String::new(),
            api: "openai-completions".into(),
            api_key: None,
            auth_header: Some(false),
            headers: HashMap::new(),
            models: Vec::new(),
            compat: crate::config::OpenAiCompatibility::default(),
        },
    );
    config.subagent_model = "broken:model".into();
    let error = resolve_model(&config, "local:parent", None)
        .expect_err("unusable provider models must fail fast");
    assert!(
        error.contains("'broken:model' is not usable"),
        "unexpected error: {error}"
    );
}

#[test]
fn unknown_id_errors_include_known_ids() {
    let manager = SubagentManager::new("session".into(), 3);
    let error = manager
        .list(Some("sa-9"))
        .expect_err("unknown ID should fail");
    assert!(error.contains("known IDs: (none)"));
}

#[test]
fn id_lists_reject_duplicates() {
    let ids = vec!["sa-1".to_string(), "sa-1".to_string()];
    assert!(validate_id_list(&ids).is_err());
}

#[test]
fn omitted_wait_timeout_has_no_deadline() {
    assert_eq!(wait_timeout(None), Ok(None));
    assert_eq!(wait_timeout(Some(7)), Ok(Some(Duration::from_secs(7))));
}

#[test]
fn wait_without_timeout_blocks_until_every_selected_subagent_settles() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
    let base_url = format!(
        "http://{}/v1",
        listener.local_addr().expect("test provider address")
    );
    let (ready_tx, ready_rx) = mpsc::channel();
    let (first_release_tx, first_release_rx) = mpsc::channel();
    let (second_release_tx, second_release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut first, _) = listener.accept().expect("first provider connection");
        read_request(&mut first).expect("first provider request");
        ready_tx.send(()).expect("first provider ready");

        let (mut second, _) = listener.accept().expect("second provider connection");
        read_request(&mut second).expect("second provider request");
        ready_tx.send(()).expect("second provider ready");

        first_release_rx.recv().expect("first provider release");
        write_response(&mut first, "first result").expect("first provider response");
        second_release_rx.recv().expect("second provider release");
        write_response(&mut second, "second result").expect("second provider response");
    });
    let manager = SubagentManager::new("session".into(), 2);
    let config = provider_config(base_url);
    let first = manager
        .spawn(
            config.clone(),
            "local:model",
            Some("first"),
            "first task",
            None,
        )
        .expect("first subagent spawn");
    let second = manager
        .spawn(config, "local:model", Some("second"), "second task", None)
        .expect("second subagent spawn");
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first worker should reach the provider");
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("second worker should reach the provider");

    let (wait_tx, wait_rx) = mpsc::channel();
    let wait_manager = manager.clone();
    let wait = std::thread::spawn(move || {
        let result = wait_manager.wait(&[first.to_string(), second.to_string()], None);
        wait_tx.send(result).expect("wait result receiver");
    });

    first_release_tx.send(()).expect("release first provider");
    assert!(
        wait_rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "the wait must remain blocked while any selected subagent is active"
    );

    second_release_tx.send(()).expect("release second provider");
    let output = wait_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("wait should finish after both providers")
        .expect("wait result");
    assert!(output.contains("first result"));
    assert!(output.contains("second result"));
    assert!(!output.contains("Wait ended after"));

    wait.join().expect("wait thread should exit");
    server.join().expect("provider server should exit");
    manager.shutdown_and_discard();
}

#[test]
fn omitted_spawn_names_are_generated_and_supplied_names_are_kept() {
    let config = provider_config("http://127.0.0.1:9/v1".into());
    let manager = SubagentManager::new("session".into(), 3);
    manager
        .spawn(config.clone(), "local:model", None, "task one", None)
        .expect("spawn without a name");
    manager
        .spawn(config, "local:model", None, "task two", None)
        .expect("second spawn without a name");
    manager
        .spawn(
            provider_config("http://127.0.0.1:9/v1".into()),
            "local:model",
            Some("  custom  "),
            "task three",
            None,
        )
        .expect("spawn with an explicit name");

    let snapshots = manager.snapshots();
    let names = snapshots
        .iter()
        .map(|snapshot| snapshot.name.as_str())
        .collect::<Vec<_>>();
    assert!(
        names.contains(&"custom"),
        "supplied names should be trimmed and kept; got {names:?}"
    );
    let generated = names
        .iter()
        .filter(|name| **name != "custom")
        .collect::<Vec<_>>();
    assert_eq!(generated.len(), 2);
    assert!(
        generated[0] != generated[1],
        "generated handles must be unique; got {names:?}"
    );
    assert!(generated.iter().all(|name| !name.is_empty()));
    manager.shutdown_and_discard();
}

#[test]
fn active_capacity_and_queue_limits_are_reserved_synchronously() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
    let base_url = format!(
        "http://{}/v1",
        listener.local_addr().expect("test provider address")
    );
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("subagent provider connection");
        read_request(&mut stream).expect("subagent provider request");
        ready_tx.send(()).expect("provider ready signal");
        release_rx.recv().expect("provider release signal");
        write_response(&mut stream, "done").expect("subagent provider response");
    });
    let config = provider_config(base_url);
    let manager = SubagentManager::new("session".into(), 1);
    let id = manager
        .spawn(config.clone(), "local:model", Some("first"), "work", None)
        .expect("first spawn should reserve the only slot");
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first worker should reach the provider");

    let error = manager
        .spawn(config, "local:model", Some("second"), "work", None)
        .expect_err("a simultaneous spawn must not exceed capacity");
    assert!(error.contains("capacity is full"));
    for index in 0..MAX_QUEUE_MESSAGES {
        manager
            .send(id.as_str(), &format!("queued {index}"), RunOrigin::Model)
            .expect("messages through the queue limit should be accepted");
    }
    assert!(
        manager
            .send(id.as_str(), "one too many", RunOrigin::Model)
            .is_err()
    );

    manager.interrupt_all();
    release_tx.send(()).expect("release provider response");
    manager
        .wait(&[id.to_string()], Some(5))
        .expect("canceled worker should settle");
    server.join().expect("provider server should exit");
    manager.shutdown_and_discard();
}

#[test]
fn timed_out_wait_reports_progress_and_discourages_cancellation() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
    let base_url = format!(
        "http://{}/v1",
        listener.local_addr().expect("test provider address")
    );
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("subagent provider connection");
        read_request(&mut stream).expect("subagent provider request");
        ready_tx.send(()).expect("provider ready signal");
        release_rx.recv().expect("provider release signal");
        write_response(&mut stream, "slow result").expect("subagent provider response");
    });
    let manager = SubagentManager::new("session".into(), 1);
    let id = manager
        .spawn(
            provider_config(base_url),
            "local:model",
            Some("slow"),
            "long work",
            None,
        )
        .expect("slow subagent spawn");
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("worker should reach the provider");

    let timed_out = manager
        .wait(&[id.to_string()], Some(1))
        .expect("a timed-out wait must still report status");
    assert!(
        timed_out.contains("1 of 1 subagent(s) still running"),
        "the timed-out wait must explain that the run continues; got:\n{timed_out}"
    );
    assert!(
        timed_out.contains("Do not cancel a subagent because a wait timed out"),
        "the timed-out wait must discourage premature cancellation; got:\n{timed_out}"
    );

    release_tx.send(()).expect("release provider response");
    let settled = manager
        .wait(&[id.to_string()], Some(5))
        .expect("released subagent should settle");
    assert!(settled.contains("slow result"));
    assert!(
        !settled.contains("still running"),
        "settled waits must not carry the timeout notice; got:\n{settled}"
    );
    server.join().expect("provider server should exit");
    manager.shutdown_and_discard();
}

#[test]
fn interrupted_wait_explains_that_the_work_continues() {
    crate::set_interrupted(false);
    let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
    let base_url = format!(
        "http://{}/v1",
        listener.local_addr().expect("test provider address")
    );
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("subagent provider connection");
        read_request(&mut stream).expect("subagent provider request");
        ready_tx.send(()).expect("provider ready signal");
        release_rx.recv().expect("provider release signal");
        write_response(&mut stream, "slow result").expect("subagent provider response");
    });
    let manager = SubagentManager::new("session".into(), 1);
    let id = manager
        .spawn(
            provider_config(base_url),
            "local:model",
            Some("slow"),
            "long work",
            None,
        )
        .expect("slow subagent spawn");
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("worker should reach the provider");

    let token = CancellationToken::default();
    token.cancel();
    crate::cancellation::scope(&token, || {
        let unbounded = manager
            .wait(&[id.to_string()], None)
            .expect("an interrupted wait must still report status");
        assert!(
            unbounded.contains("Wait interrupted"),
            "the interrupted wait must explain that the wait ended; got:\n{unbounded}"
        );
        assert!(
            unbounded.contains("1 of 1 subagent(s) still running"),
            "the interrupted wait must report that the run continues; got:\n{unbounded}"
        );
        assert!(
            unbounded.contains("Do not wait again"),
            "the interrupted wait must discourage immediately waiting again; got:\n{unbounded}"
        );
        assert!(
            !unbounded.contains("Wait ended after"),
            "an interrupted wait must not look like a timeout; got:\n{unbounded}"
        );

        let timed = manager
            .wait(&[id.to_string()], Some(300))
            .expect("interrupt should win over a pending timeout");
        assert!(
            timed.contains("Wait interrupted"),
            "Esc during a timed wait is a cancel, not a timeout; got:\n{timed}"
        );
        assert!(
            !timed.contains("Wait ended after"),
            "interrupt must take precedence over timeout; got:\n{timed}"
        );
    });

    release_tx.send(()).expect("release provider response");
    manager
        .wait(&[id.to_string()], Some(5))
        .expect("released subagent should settle");
    server.join().expect("provider server should exit");
    manager.shutdown_and_discard();
}

#[test]
fn interrupted_wait_stays_quiet_when_every_id_already_settled() {
    crate::set_interrupted(false);
    let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
    let base_url = format!(
        "http://{}/v1",
        listener.local_addr().expect("test provider address")
    );
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("subagent provider connection");
        read_request(&mut stream).expect("subagent provider request");
        write_response(&mut stream, "done").expect("subagent provider response");
    });
    let manager = SubagentManager::new("session".into(), 1);
    let id = manager
        .spawn(
            provider_config(base_url),
            "local:model",
            Some("quick"),
            "short work",
            None,
        )
        .expect("subagent spawn");
    manager
        .wait(&[id.to_string()], Some(5))
        .expect("subagent should settle");

    let token = CancellationToken::default();
    token.cancel();
    crate::cancellation::scope(&token, || {
        let output = manager
            .wait(&[id.to_string()], None)
            .expect("waiting on a settled ID should succeed");
        assert!(
            !output.contains("Wait interrupted"),
            "a finished wait must not claim it was interrupted; got:\n{output}"
        );
        assert!(
            !output.contains("Wait ended after"),
            "a finished wait must not carry a timeout notice; got:\n{output}"
        );
        assert!(output.contains("done"));
    });

    server.join().expect("provider server should exit");
    manager.shutdown_and_discard();
}

#[test]
fn canceling_an_idle_restart_releases_its_capacity() {
    let manager = SubagentManager::new("session".into(), 1);
    {
        let mut state = manager.lock();
        state.active = 1;
        state.entries.push(test_entry(SubagentStatus::Canceling));
    }
    let id = SubagentId::new(1);
    let worker_manager = manager.clone();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = worker_manager.wait_for_work(&id);
        done_tx.send(()).expect("worker completion receiver");
    });

    let returned_promptly = done_rx.recv_timeout(Duration::from_millis(200)).is_ok();
    if !returned_promptly {
        manager.lock().shutting_down = true;
        manager.shared.changed.notify_all();
    }
    worker.join().expect("idle worker should stop");
    let state = manager.lock();
    let active = state.active;
    let status = state.entries[0].snapshot.status;
    let outcome = state.entries[0].snapshot.latest_outcome;
    drop(state);
    manager.shutdown_and_discard();

    assert!(
        returned_promptly,
        "canceling idle workers must not wait for new work"
    );
    assert_eq!(active, 0);
    assert_eq!(status, SubagentStatus::Done);
    assert_eq!(outcome, Some(RunOutcome::Interrupted));
}

#[test]
fn work_canceled_after_dequeue_does_not_enter_the_transcript() {
    let manager = SubagentManager::new("session".into(), 1);
    manager
        .lock()
        .entries
        .push(test_entry(SubagentStatus::Canceling));
    let work = WorkItem {
        message: "canceled follow-up".into(),
        origin: RunOrigin::PrivateUser,
        run_number: 2,
    };

    let started = manager.begin_work(&SubagentId::new(1), &work);
    let state = manager.lock();
    let snapshot = &state.entries[0].snapshot;

    assert!(!started);
    assert_eq!(snapshot.status, SubagentStatus::Done);
    assert_eq!(snapshot.latest_outcome, Some(RunOutcome::Interrupted));
    assert!(snapshot.transcript.is_empty());
}

#[test]
fn settled_agents_reuse_their_conversation_on_restart() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
    let base_url = format!(
        "http://{}/v1",
        listener.local_addr().expect("test provider address")
    );
    let server = std::thread::spawn(move || {
        for response in ["first", "second"] {
            let (mut stream, _) = listener.accept().expect("subagent provider connection");
            read_request(&mut stream).expect("subagent provider request");
            write_response(&mut stream, response).expect("subagent provider response");
        }
    });
    let config = provider_config(base_url);
    let manager = SubagentManager::new("session".into(), 1);
    let id = manager
        .spawn(config, "local:model", Some("reused"), "first task", None)
        .expect("initial subagent spawn");
    manager
        .wait(&[id.to_string()], Some(5))
        .expect("initial subagent run should settle");
    let first_thread = manager
        .lock()
        .entries
        .iter()
        .find(|entry| entry.snapshot.id == id)
        .and_then(|entry| entry.thread_id)
        .expect("settled subagent should retain its worker");
    manager
        .send(id.as_str(), "follow-up", RunOrigin::PrivateUser)
        .expect("settled subagent should restart");
    manager
        .wait(&[id.to_string()], Some(5))
        .expect("restarted subagent should settle");

    let snapshot = manager
        .snapshots()
        .into_iter()
        .find(|snapshot| snapshot.id == id)
        .expect("retained subagent snapshot");
    let second_thread = manager
        .lock()
        .entries
        .iter()
        .find(|entry| entry.snapshot.id == id)
        .and_then(|entry| entry.thread_id)
        .expect("restarted subagent should retain its worker");
    assert_eq!(first_thread, second_thread);
    assert_eq!(snapshot.completed_turns, 2);
    assert!(snapshot.transcript.iter().any(|item| {
        matches!(item.as_ref(), SubagentTranscriptItem::Assistant(text) if text == "first")
    }));
    assert_eq!(snapshot.latest_final_result, "second");
    server.join().expect("provider server should exit");
    manager.shutdown_and_discard();
}

#[test]
fn queued_messages_run_in_order_on_one_worker() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
    let base_url = format!(
        "http://{}/v1",
        listener.local_addr().expect("test provider address")
    );
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        for (index, response) in ["first", "second", "third"].into_iter().enumerate() {
            let (mut stream, _) = listener.accept().expect("subagent provider connection");
            read_request(&mut stream).expect("subagent provider request");
            if index == 0 {
                ready_tx.send(()).expect("provider ready signal");
                release_rx.recv().expect("provider release signal");
            }
            write_response(&mut stream, response).expect("subagent provider response");
        }
    });
    let manager = SubagentManager::new("session".into(), 1);
    let id = manager
        .spawn(
            provider_config(base_url),
            "local:model",
            Some("ordered"),
            "first task",
            None,
        )
        .expect("initial subagent spawn");
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first worker should reach the provider");
    manager
        .send(id.as_str(), "second task", RunOrigin::Model)
        .expect("second task should queue");
    manager
        .send(id.as_str(), "third task", RunOrigin::PrivateUser)
        .expect("third task should queue");
    release_tx.send(()).expect("release provider response");
    manager
        .wait(&[id.to_string()], Some(5))
        .expect("queued work should settle");

    let snapshot = manager
        .snapshots()
        .into_iter()
        .find(|snapshot| snapshot.id == id)
        .expect("ordered subagent snapshot");
    let messages = snapshot
        .transcript
        .iter()
        .filter_map(|item| match item.as_ref() {
            SubagentTranscriptItem::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(messages, ["first task", "second task", "third task"]);
    assert_eq!(snapshot.completed_turns, 3);
    assert!(snapshot.queued_messages.is_empty());
    server.join().expect("provider server should exit");
    manager.shutdown_and_discard();
}

#[test]
fn pruning_keeps_active_waited_and_undelivered_entries() {
    let manager = SubagentManager::new("session".into(), 3);
    let mut state = manager.lock();
    for sequence in 1..=MAX_TRACKED_SUBAGENTS as u64 {
        let conversation =
            Conversation::memory(config(), "parent".into(), format!("session-sa-{sequence}"));
        let cancellation = conversation.cancellation_token();
        let mut snapshot = SubagentSnapshot::new(
            SubagentId::new(sequence),
            format!("agent {sequence}"),
            "default".into(),
            "task".into(),
            "parent".into(),
            100,
        );
        snapshot.status = SubagentStatus::Done;
        snapshot.settled_at = Some(
            Instant::now()
                .checked_sub(Duration::from_secs(MAX_TRACKED_SUBAGENTS as u64 - sequence))
                .expect("test settlement instant"),
        );
        state.entries.push(Entry {
            snapshot,
            cancellation,
            work: VecDeque::new(),
            next_run_number: 2,
            thread_id: None,
            handle: None,
            wait_interest: 0,
            pending_delivery: Vec::new(),
            suppress_delivery: false,
            steers: crate::agent::SteerInbox::default(),
        });
    }
    state.deferred.push_back(DeferredResult {
        id: SubagentId::new(1),
        name: "agent 1".into(),
        run_number: 1,
        outcome: RunOutcome::Completed,
        result: "done".into(),
        error: String::new(),
        sequence: 1,
    });
    state.entries[1].wait_interest = 1;
    state.entries[2].snapshot.status = SubagentStatus::Canceling;

    prune_settled(&mut state);

    assert_eq!(state.entries.len(), MAX_TRACKED_SUBAGENTS - 1);
    assert!(
        state
            .entries
            .iter()
            .any(|entry| entry.snapshot.id.as_str() == "sa-1")
    );
    assert!(
        state
            .entries
            .iter()
            .any(|entry| entry.snapshot.id.as_str() == "sa-2")
    );
    assert!(
        state
            .entries
            .iter()
            .any(|entry| entry.snapshot.id.as_str() == "sa-3")
    );
    assert!(
        !state
            .entries
            .iter()
            .any(|entry| entry.snapshot.id.as_str() == "sa-4")
    );
}

#[test]
fn late_wait_consumes_deferred_results_and_private_runs_stay_private() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
    let base_url = format!(
        "http://{}/v1",
        listener.local_addr().expect("test provider address")
    );
    let server = std::thread::spawn(move || {
        for response in ["model result", "private result"] {
            let (mut stream, _) = listener.accept().expect("subagent provider connection");
            read_request(&mut stream).expect("subagent provider request");
            write_response(&mut stream, response).expect("subagent provider response");
        }
    });
    let config = provider_config(base_url);
    let manager = SubagentManager::new("session".into(), 1);
    let id = manager
        .spawn(config, "local:model", Some("delivery"), "model task", None)
        .expect("model-originated run should start");
    let deadline = Instant::now() + Duration::from_secs(5);
    while manager
        .snapshots()
        .iter()
        .any(|snapshot| snapshot.id == id && snapshot.status.is_active())
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(manager.has_deferred());

    let waited = manager
        .wait(&[id.to_string()], Some(1))
        .expect("late wait should read the settled result");
    assert!(waited.contains("model result"));
    assert!(!manager.has_deferred());

    manager
        .send(id.as_str(), "private follow-up", RunOrigin::PrivateUser)
        .expect("private takeover should restart the agent");
    manager
        .wait(&[id.to_string()], Some(5))
        .expect("private run should settle");
    assert!(!manager.has_deferred());
    server.join().expect("provider server should exit");
    manager.shutdown_and_discard();
}

#[test]
fn private_cancellation_preserves_an_earlier_model_result() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
    let base_url = format!(
        "http://{}/v1",
        listener.local_addr().expect("test provider address")
    );
    let (first_ready_tx, first_ready_rx) = mpsc::channel();
    let (first_release_tx, first_release_rx) = mpsc::channel();
    let (private_ready_tx, private_ready_rx) = mpsc::channel();
    let (private_release_tx, private_release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut first, _) = listener.accept().expect("first provider connection");
        read_request(&mut first).expect("first provider request");
        first_ready_tx.send(()).expect("first provider ready");
        first_release_rx.recv().expect("first provider release");
        write_response(&mut first, "model result").expect("first provider response");

        let (mut private, _) = listener.accept().expect("private provider connection");
        read_request(&mut private).expect("private provider request");
        private_ready_tx.send(()).expect("private provider ready");
        private_release_rx.recv().expect("private provider release");
        // Cancellation may already have closed the client connection.
        let _ = write_response(&mut private, "private result");
    });
    let manager = SubagentManager::new("session".into(), 1);
    let id = manager
        .spawn(
            provider_config(base_url),
            "local:model",
            Some("mixed origin"),
            "model task",
            None,
        )
        .expect("model subagent spawn");
    first_ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("model run should reach provider");
    manager
        .send(id.as_str(), "private task", RunOrigin::PrivateUser)
        .expect("private task should queue");
    first_release_tx.send(()).expect("release model response");
    private_ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("private run should reach provider");

    let cancel_manager = manager.clone();
    let cancel_id = id.to_string();
    let cancel = std::thread::spawn(move || cancel_manager.cancel(&[cancel_id], true));
    // Once the wake handler is installed, cancellation may interrupt the
    // provider read and pass through the transient Canceling state before
    // this thread can observe it. Releasing the server remains safe in
    // either ordering.
    private_release_tx
        .send(())
        .expect("release private provider response");
    cancel
        .join()
        .expect("private cancellation thread")
        .expect("private cancellation result");

    let deferred = manager.drain_deferred();
    assert_eq!(deferred.len(), 1);
    assert_eq!(deferred[0].result, "model result");
    server.join().expect("provider server should exit");
    manager.shutdown_and_discard();
}

#[test]
fn wait_all_returns_immediately_without_active_subagents() {
    let manager = SubagentManager::new("session".into(), 3);
    assert_eq!(manager.active_count(), 0);
    assert!(manager.wait_all(1));
    manager.shutdown_and_discard();
}

#[test]
fn preset_spawns_apply_the_preset_model_and_agent_label() {
    // Port 9 refuses connections instantly, so the child settles Failed
    // without a server; the assertions are about spawn-time effects.
    let config = provider_config("http://127.0.0.1:9/v1".into());
    let preset = super::super::presets::bundled().remove(0);
    let manager = SubagentManager::new("session".into(), 1);
    let id = manager
        .spawn(config, "local:model", None, "find the bug", Some(&preset))
        .expect("preset spawn");
    manager
        .wait(&[id.to_string()], Some(10))
        .expect("preset child settles");

    let snapshot = manager
        .snapshots()
        .into_iter()
        .find(|snapshot| snapshot.id == id)
        .expect("preset child snapshot");
    assert_eq!(snapshot.agent, "scout");
    assert_eq!(snapshot.model, "local:model", "scout inherits by default");
    manager.shutdown_and_discard();
}

#[test]
fn preset_model_overrides_config() {
    let mut config = provider_config("http://127.0.0.1:9/v1".into());
    config.subagent_model = "local:configured".into();
    let mut preset = super::super::presets::bundled().remove(0);
    preset.model = Some("local:fast".into());

    let via_preset = resolve_model(&config, "local:parent", preset.model.as_deref())
        .expect("preset model applies");
    assert_eq!(via_preset, "local:fast");

    preset.model = None;
    let via_config =
        resolve_model(&config, "local:parent", preset.model.as_deref()).expect("config applies");
    assert_eq!(via_config, "local:configured");
}

#[test]
fn request_budget_steers_then_stops_a_runaway_subagent() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
    let base_url = format!(
        "http://{}/v1",
        listener.local_addr().expect("test provider address")
    );
    let server = std::thread::spawn(move || {
        let mut bodies = Vec::new();
        for index in 0..3 {
            let (mut stream, _) = listener.accept().expect("subagent provider connection");
            let body = read_request(&mut stream).expect("subagent provider request");
            bodies.push(body);
            write_tool_call_response(&mut stream, "working", &format!("call-{index}"))
                .expect("subagent provider response");
        }
        let (mut stream, _) = listener.accept().expect("follow-up connection");
        read_request(&mut stream).expect("follow-up request");
        write_response(&mut stream, "finished follow-up").expect("follow-up response");
        bodies
    });
    let mut config = provider_config(base_url);
    config.subagent_request_budget = 2;
    let manager = SubagentManager::new("session".into(), 1);
    let id = manager
        .spawn(
            config,
            "local:model",
            Some("runaway"),
            "keep working forever",
            None,
        )
        .expect("runaway subagent spawn");
    assert!(manager.wait_all(10));
    let deliveries = manager.drain_deferred();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].outcome, RunOutcome::Interrupted);
    assert_eq!(deliveries[0].error, "subagent request budget exceeded");
    let waited = manager
        .wait(&[id.to_string()], Some(10))
        .expect("budget-stopped subagent should settle");
    assert!(waited.contains("subagent request budget exceeded"));
    assert_eq!(
        manager.snapshots()[0].error,
        "subagent request budget exceeded"
    );

    assert!(
        waited.contains("[cancelled after 3 requests]"),
        "the hard stop must deliver a salvage envelope; got:\n{waited}"
    );
    let snapshot = manager
        .snapshots()
        .into_iter()
        .find(|snapshot| snapshot.id == id)
        .expect("runaway subagent snapshot");
    assert_eq!(snapshot.requests, 3);
    assert_eq!(snapshot.run_tokens, 36);
    assert_eq!(manager.total_child_tokens(), 36);
    manager
        .send(id.as_str(), "finish the follow-up", RunOrigin::Model)
        .expect("restart after budget stop");
    assert!(manager.wait_all(5));
    let deliveries = manager.drain_deferred();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].outcome, RunOutcome::Completed);
    assert!(deliveries[0].error.is_empty());
    assert!(manager.snapshots()[0].error.is_empty());
    let bodies = server.join().expect("provider server should exit");
    assert_eq!(bodies.len(), 3, "the fourth request must never happen");
    assert!(
        bodies[2].contains("Request budget reached"),
        "the steer message must ride the final request; got:\n{}",
        bodies[2]
    );
    manager.shutdown_and_discard();
}

#[test]
fn run_timeout_interrupts_an_in_flight_subagent_request() {
    crate::install_interrupt_handler().expect("interrupt handler");
    let listener = TcpListener::bind("127.0.0.1:0").expect("test provider listener");
    let base_url = format!(
        "http://{}/v1",
        listener.local_addr().expect("test provider address")
    );
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("subagent provider connection");
        read_request(&mut stream).expect("subagent provider request");
        std::thread::sleep(Duration::from_secs(3));
        // The timeout should close or interrupt the client before this
        // response. A broken pipe is therefore an expected outcome.
        let _ = write_tool_call_response(&mut stream, "too late", "call-0");
    });
    let mut config = provider_config(base_url);
    config.subagent_timeout_secs = 1;
    let manager = SubagentManager::new("session".into(), 1);
    let started = Instant::now();
    let id = manager
        .spawn(config, "local:model", Some("slow"), "slow work", None)
        .expect("slow subagent spawn");
    assert!(manager.wait_all(10));
    let deliveries = manager.drain_deferred();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].outcome, RunOutcome::Interrupted);
    assert_eq!(deliveries[0].error, "subagent timeout exceeded");
    let waited = manager
        .wait(&[id.to_string()], Some(10))
        .expect("timed-out subagent should settle");
    assert!(waited.contains("subagent timeout exceeded"));
    assert_eq!(manager.snapshots()[0].error, "subagent timeout exceeded");

    assert!(
        waited.contains("[cancelled after"),
        "the timeout stop must deliver a salvage marker; got:\n{waited}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the watchdog must interrupt the in-flight provider request"
    );
    server.join().expect("provider server should exit");
    manager.shutdown_and_discard();
}

#[test]
fn canceled_steers_do_not_restart_work_or_consume_capacity() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let config = provider_config(format!(
        "http://{}/v1",
        listener.local_addr().expect("address")
    ));
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("connection");
        read_request(&mut stream).expect("request");
        ready_tx.send(()).expect("ready");
        release_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("release");
        let _ = write_response(&mut stream, "done");
        let (mut follow_up, _) = listener.accept().expect("follow-up connection");
        let request = read_request(&mut follow_up).expect("follow-up request");
        write_response(&mut follow_up, "follow-up answer").expect("follow-up response");
        request
    });
    let manager = SubagentManager::new("session".into(), 1);
    let id = manager
        .spawn(config, "local:model", None, "task", None)
        .expect("spawn");
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("request started");
    manager
        .steer(id.as_str(), "first steer")
        .expect("first steer");
    manager
        .steer(id.as_str(), "second steer")
        .expect("second steer");
    // Cancel while the request is blocked so both steers remain unaccepted.
    let cancel_manager = manager.clone();
    let cancel_id = id.to_string();
    let cancel = std::thread::spawn(move || cancel_manager.cancel(&[cancel_id], false));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = manager.lock();
        if state.entries[0].snapshot.status != SubagentStatus::Running {
            break;
        }
        drop(state);
        assert!(Instant::now() < deadline, "cancellation did not start");
        std::thread::yield_now();
    }
    release_tx.send(()).expect("release response");
    cancel
        .join()
        .expect("cancel thread")
        .expect("cancel result");
    {
        let state = manager.lock();
        let entry = &state.entries[0];
        assert!(entry.work.is_empty(), "canceled work was requeued");
        assert!(entry.snapshot.queued_messages.is_empty());
        assert_eq!(entry.snapshot.completed_turns, 1);
        assert_eq!(entry.snapshot.status, SubagentStatus::Done);
        assert_eq!(state.active, 0);
        assert!(
            entry.snapshot.error.is_empty(),
            "user cancellation is not a limit stop"
        );
    }
    manager
        .send(id.as_str(), "authorized follow-up", RunOrigin::Model)
        .expect("capacity released for restart");
    let waited = manager
        .wait(&[id.to_string()], Some(5))
        .expect("follow-up settled");
    assert!(waited.contains("follow-up answer"));
    let request = server.join().expect("server");
    assert!(request.contains("authorized follow-up"));
    assert!(!request.contains("first steer"));
    assert!(!request.contains("second steer"));
    assert_eq!(manager.active_count(), 0);
    manager.shutdown_and_discard();
}

#[test]
fn interrupted_follow_up_does_not_salvage_a_previous_answer() {
    let mut snapshot = test_entry(SubagentStatus::Done).snapshot;
    snapshot.begin_turn("first task", RunOrigin::Model, 1);
    snapshot.apply_event(crate::agent::TurnEvent::TextDelta("previous answer"));
    snapshot.finish_turn(RunOutcome::Completed, "previous answer", None);
    snapshot.begin_turn("follow-up", RunOrigin::Model, 2);
    snapshot.finish_turn(RunOutcome::Interrupted, "", None);
    assert_eq!(
        salvage_result(&snapshot, ""),
        "[cancelled after 0 requests]"
    );
    snapshot.push_transcript(SubagentTranscriptItem::Assistant("current partial".into()));
    let salvage = salvage_result(&snapshot, "");
    assert!(salvage.contains("current partial"));
    assert!(!salvage.contains("previous answer"));
}
