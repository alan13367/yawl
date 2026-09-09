//! Subagent snapshot and deferred-result presentation.
//!
//! The manager owns capacity, execution, waiting, and delivery; this child
//! formats settled snapshots and deferred results for orchestrator display.

use std::time::{Duration, Instant};

use super::super::types::{
    MAX_FINAL_RESULT_BYTES, RunOutcome, SubagentSnapshot, SubagentTranscriptItem, bounded,
};
use super::DeferredResult;

const SALVAGE_SNIPPET_BYTES: usize = 500;

pub(super) fn format_snapshots(snapshots: &[SubagentSnapshot]) -> String {
    let mut output = String::new();
    for snapshot in snapshots {
        if !output.is_empty() {
            output.push_str("\n\n");
        }
        output.push_str(&format_detailed_snapshot(snapshot));
    }
    output
}

pub(super) fn format_deferred_result(delivery: &DeferredResult) -> String {
    let status = match delivery.outcome {
        RunOutcome::Completed => "completed",
        RunOutcome::Failed => "failed",
        RunOutcome::Interrupted => "interrupted",
    };
    let body = if delivery.error.is_empty() {
        delivery.result.clone()
    } else if delivery.result.is_empty() {
        delivery.error.clone()
    } else {
        format!("{}\n\nError: {}", delivery.result, delivery.error)
    };
    format!(
        "{} [{}] {} (run {})\n{}",
        delivery.id, status, delivery.name, delivery.run_number, body
    )
}

pub(super) fn format_detailed_snapshot(snapshot: &SubagentSnapshot) -> String {
    let percentage = snapshot
        .context_tokens
        .saturating_mul(100)
        .checked_div(snapshot.context_window)
        .unwrap_or(0);
    let mut output = format!(
        "{} [{}] {}\nagent: {}\nmodel: {}\ninitial task: {}\ncontext: {}% ({}/{})\nelapsed: {}\nactivity: {}\noutcome: {}\nturns: {}\nrequests: {}\nqueued: {}",
        snapshot.id,
        snapshot.status.label(),
        snapshot.name,
        snapshot.agent,
        snapshot.model,
        bounded(&snapshot.initial_prompt.replace('\n', " "), 240),
        percentage,
        snapshot.context_tokens,
        snapshot.context_window,
        format_duration(snapshot.elapsed(Instant::now())),
        if snapshot.current_activity.is_empty() {
            "idle"
        } else {
            &snapshot.current_activity
        },
        match snapshot.latest_outcome {
            Some(RunOutcome::Completed) => "completed",
            Some(RunOutcome::Failed) => "failed",
            Some(RunOutcome::Interrupted) => "interrupted",
            None => "pending",
        },
        snapshot.completed_turns,
        snapshot.requests,
        snapshot.queued_messages.len()
    );
    if !snapshot.error.is_empty() {
        output.push_str("\nerror: ");
        output.push_str(&snapshot.error);
    }
    let result = final_output(snapshot);
    if !result.is_empty() {
        output.push_str("\nresult:\n");
        output.push_str(&result);
    }
    output
}

/// The text reported to the orchestrator once a run settles: the complete
/// final response, the live partial answer while streaming, or the error.
fn final_output(snapshot: &SubagentSnapshot) -> String {
    let source = if !snapshot.live_assistant.is_empty() {
        &snapshot.live_assistant
    } else if !snapshot.latest_final_result.is_empty() {
        &snapshot.latest_final_result
    } else {
        &snapshot.error
    };
    source.clone()
}

/// Interrupted runs deliver a salvage envelope instead of a possibly empty
/// final result: the request count plus the last assistant text, so partial
/// work still reaches the parent.
pub(super) fn salvage_result(snapshot: &SubagentSnapshot, final_result: &str) -> String {
    let requests = snapshot.requests;
    let result = final_result.trim();
    if !result.is_empty() {
        return bounded(
            &format!("[cancelled after {requests} requests]\n{result}"),
            MAX_FINAL_RESULT_BYTES,
        );
    }
    let last_activity = snapshot
        .transcript
        .iter()
        .rev()
        .take_while(|item| !matches!(item.as_ref(), SubagentTranscriptItem::User { .. }))
        .find_map(|item| match item.as_ref() {
            SubagentTranscriptItem::Assistant(text) => Some(text.as_str()),
            _ => None,
        });
    match last_activity {
        Some(text) if !text.trim().is_empty() => bounded(
            &format!(
                "[cancelled after {requests} requests — last activity: \"{}\"]",
                bounded(text.trim(), SALVAGE_SNIPPET_BYTES)
            ),
            MAX_FINAL_RESULT_BYTES,
        ),
        _ => format!("[cancelled after {requests} requests]"),
    }
}

pub(super) fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3600 {
        format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::types::{MAX_TRACKED_SUBAGENTS, SubagentId, SubagentStatus};
    use super::*;

    #[test]
    fn wait_format_reports_every_id_with_complete_results() {
        let snapshots = (1..=MAX_TRACKED_SUBAGENTS as u64)
            .map(|sequence| {
                let mut snapshot = SubagentSnapshot::new(
                    SubagentId::new(sequence),
                    format!("agent {sequence}"),
                    "default".into(),
                    "p".repeat(240),
                    "model".into(),
                    100,
                );
                snapshot.latest_final_result = format!("result {sequence} tail");
                snapshot.status = SubagentStatus::Done;
                snapshot
            })
            .collect::<Vec<_>>();

        let output = format_snapshots(&snapshots);

        for sequence in 1..=MAX_TRACKED_SUBAGENTS as u64 {
            assert!(output.contains(&format!("sa-{sequence} [done]")));
            assert!(output.contains(&format!("result {sequence} tail")));
        }
    }

    #[test]
    fn salvage_envelope_reports_requests_and_last_activity() {
        let mut snapshot = SubagentSnapshot::new(
            SubagentId::new(1),
            "agent".into(),
            "default".into(),
            "prompt".into(),
            "local:model".into(),
            100,
        );
        snapshot.requests = 4;
        assert_eq!(
            salvage_result(&snapshot, ""),
            "[cancelled after 4 requests]",
            "an empty transcript delivers the bare marker"
        );
        snapshot.push_transcript(SubagentTranscriptItem::Assistant("found the bug".into()));
        assert_eq!(
            salvage_result(&snapshot, ""),
            "[cancelled after 4 requests — last activity: \"found the bug\"]"
        );
        assert_eq!(
            salvage_result(&snapshot, "full result"),
            "[cancelled after 4 requests]\nfull result"
        );
    }
}
