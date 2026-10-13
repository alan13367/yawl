//! Append-only JSONL session persistence, one `<id>.jsonl` file per session
//! in a caller-chosen directory. The config facade resolves the per-project
//! layout under `~/.yawl/sessions/`; this module stays layout-agnostic.
//!
//! The file keeps the full original history forever; compaction is recorded
//! as an event and applied at replay time. A session owns the history it
//! records: every change is an event, written first and then applied by the
//! one reducer in [`state`], so replaying the log rebuilds exactly the live
//! state. Memory-only sessions apply the same events without a file.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::Error;
use crate::provider::{Message, Role, TokenUsage, UsageSummary};

mod index;
mod plans;
mod recovery;
mod state;
pub(crate) use recovery::PendingUndo;
#[cfg(test)]
pub(crate) use recovery::missing_tool_results;
use state::SessionState;

/// Planning state retained with a session, outside the working tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum PlanState {
    Draft {
        objective: String,
        #[serde(default)]
        questions_asked: bool,
    },
    Ready {
        plan: String,
        #[serde(default)]
        revision: usize,
    },
}

impl PlanState {
    pub(crate) fn ready(&self) -> Option<&str> {
        match self {
            Self::Ready { plan, .. } => Some(plan),
            Self::Draft { .. } => None,
        }
    }

    pub(crate) fn questions_asked(&self) -> bool {
        matches!(
            self,
            Self::Draft {
                questions_asked: true,
                ..
            }
        )
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SessionEvent {
    Meta {
        id: String,
        created_unix: u64,
        /// Canonical working directory the session belongs to.
        cwd: String,
        /// Model active when the session started; `model_switch` events
        /// replace it.
        model: String,
    },
    Message {
        message: Message,
    },
    /// The user switched the active model mid-session; replays adopt the most
    /// recent switch so resuming continues with the last used model.
    ModelSwitch {
        model: String,
    },
    /// The user chose a reasoning effort for this session. `effort: null` is
    /// an explicit provider default. `inherit` drops the choice so later
    /// resumes follow the saved config default again. Replays keep the latest
    /// event.
    ReasoningEffort {
        effort: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        inherit: bool,
    },
    /// `replaced` messages beginning at `start` (at that point in replay)
    /// were folded into `summary`. Older logs omit `start` and default to the
    /// original prefix-compaction behavior. With `plan_revision`, the
    /// summary is the implementation handoff for that plan revision.
    Compaction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan_revision: Option<usize>,
        summary: String,
        #[serde(default)]
        start: usize,
        replaced: usize,
        /// Provider-native replacement history for compatible resumed turns.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        provider_data: Vec<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_data_model: Option<String>,
    },
    /// The last `dropped` messages were removed by `/undo`.
    Undo {
        dropped: usize,
        /// Set when the dropped range includes a goal's starting message.
        #[serde(default)]
        clear_goal: bool,
        /// Planning state to restore after dropping the turn. Older logs
        /// predate planning and therefore default to no active plan.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan_state: Option<PlanState>,
    },
    UndoStarted {
        undo: PendingUndo,
    },
    UndoFinished,
    ToolResultsRecovered {
        index: usize,
        results: Vec<Message>,
    },
    Context {
        context: ContextUsage,
    },
    GoalStart {
        goal: String,
        message: Message,
    },
    GoalComplete {
        message: Message,
    },
    GoalCancel,
    PlanStart {
        objective: String,
        message: Message,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        previous: Option<PlanState>,
    },
    PlanReady {
        plan: String,
        message: Message,
    },
    PlanQuestionsAsked,
    PlanImplemented {
        message: Message,
    },
    PlanCancel,
    /// Provider-reported usage for one successful model request.
    Usage {
        usage: TokenUsage,
    },
}

/// Last measured context, separate from aggregate billing and compaction usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ContextUsage {
    pub tokens: u64,
    pub messages: usize,
    pub model: String,
    pub overhead: u64,
}

/// The log file behind a persistent session.
#[derive(Debug)]
struct Storage {
    file: File,
    /// Canonical directory holding `<id>.jsonl` and `<id>/plans/`.
    directory: PathBuf,
    /// Length to truncate back to when an earlier append failed partway.
    rollback_to: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct Session {
    pub(crate) id: String,
    /// `None` for memory-only sessions, such as subagent conversations.
    storage: Option<Storage>,
    state: SessionState,
    /// Log lines replay could not decode and skipped.
    skipped_lines: usize,
    #[cfg(test)]
    fail_after_appends: Option<usize>,
}

impl Session {
    /// Creates a new session with a timestamp-derived id, recording the
    /// working directory and model in the header event.
    pub(crate) fn create(dir: &Path, cwd: &Path, model: &str) -> Result<Session, Error> {
        fs::create_dir_all(dir)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let base_id = format!(
            "{}-{:04x}",
            format_timestamp(now.as_secs()),
            std::process::id() & 0xffff
        );
        let (id, file) = (0u32..)
            .find_map(|suffix| {
                let id = if suffix == 0 {
                    base_id.clone()
                } else {
                    format!("{base_id}-{suffix}")
                };
                let path = dir.join(format!("{id}.jsonl"));
                match OpenOptions::new().create_new(true).append(true).open(path) {
                    Ok(file) => Some(Ok((id, file))),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
                    Err(e) => Some(Err(e)),
                }
            })
            .transpose()?
            .ok_or_else(|| std::io::Error::other("could not allocate a session id"))?;
        let mut session = Session {
            id: id.clone(),
            storage: Some(Storage {
                file,
                directory: fs::canonicalize(dir)?,
                rollback_to: None,
            }),
            state: SessionState::default(),
            skipped_lines: 0,
            #[cfg(test)]
            fail_after_appends: None,
        };
        session.record(SessionEvent::Meta {
            id,
            created_unix: now.as_secs(),
            cwd: cwd.to_string_lossy().into_owned(),
            model: model.to_string(),
        })?;
        Ok(session)
    }

    /// A session that applies events in memory and writes nothing.
    pub(crate) fn memory(id: String, model: &str) -> Session {
        Session {
            id,
            storage: None,
            state: SessionState {
                model: model.to_string(),
                ..SessionState::default()
            },
            skipped_lines: 0,
            #[cfg(test)]
            fail_after_appends: None,
        }
    }

    /// Opens an existing session by id and replays its history.
    pub(crate) fn open(dir: &Path, id: &str) -> Result<Session, Error> {
        validate_id(id)?;
        let path = dir.join(format!("{id}.jsonl"));
        let replayed = replay(&path)?;
        let mut file = OpenOptions::new().read(true).append(true).open(&path)?;
        if let Some(start) = replayed.torn_tail {
            // An append that never completed; it was never applied either.
            file.set_len(start)?;
        } else if file.metadata()?.len() > 0 {
            // Separate a complete but unterminated final event from
            // subsequent JSONL events.
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                file.write_all(b"\n")?;
            }
        }
        let mut session = Session {
            id: id.to_string(),
            storage: Some(Storage {
                file,
                directory: fs::canonicalize(dir)?,
                rollback_to: None,
            }),
            state: replayed.state,
            skipped_lines: replayed.skipped_lines,
            #[cfg(test)]
            fail_after_appends: None,
        };
        if session.state.pending_undo.is_none() {
            session.repair_tool_results()?;
        }
        Ok(session)
    }

    /// Opens the most recently modified session, if any.
    pub(crate) fn open_latest(dir: &Path) -> Result<Option<Session>, Error> {
        match list(dir)?.first() {
            Some(info) => Ok(Some(Session::open(dir, &info.id)?)),
            None => Ok(None),
        }
    }

    /// Opens the unique session matching `id` across project directories.
    /// Missing files are skipped. Replay, permission, and other I/O errors
    /// are returned, and duplicate ids are rejected as ambiguous.
    pub(crate) fn open_searching(dirs: &[PathBuf], id: &str) -> Result<Session, Error> {
        validate_id(id)?;
        let mut found: Option<(PathBuf, Session)> = None;
        for dir in dirs {
            match Session::open(dir, id) {
                Ok(session) => {
                    if let Some((found_dir, _)) = &found {
                        return Err(Error::Invalid(format!(
                            "session '{id}' is ambiguous; found in '{}' and '{}'",
                            found_dir.display(),
                            dir.display()
                        )));
                    }
                    found = Some((dir.clone(), session));
                }
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        found
            .map(|(_, session)| session)
            .ok_or_else(|| Error::Invalid(format!("session '{id}' not found")))
    }

    /// The conversation history this session has recorded.
    pub(crate) fn messages(&self) -> &[Message] {
        &self.state.messages
    }

    /// Direct history access for tests that stage conversations the log
    /// never recorded.
    #[cfg(test)]
    pub(crate) fn messages_mut(&mut self) -> &mut Vec<Message> {
        &mut self.state.messages
    }

    /// A note for the user when replay skipped log lines it could not read.
    pub(crate) fn replay_warning(&self) -> Option<String> {
        (self.skipped_lines > 0).then(|| {
            let (noun, verb) = if self.skipped_lines == 1 {
                ("entry", "was")
            } else {
                ("entries", "were")
            };
            format!(
                "Session {} has {} unreadable log {noun} that {verb} skipped; \
                 the restored history may be incomplete.",
                self.id, self.skipped_lines
            )
        })
    }

    /// The model recorded at creation, replaced by the most recent switch.
    #[cfg(test)]
    pub(crate) fn model(&self) -> &str {
        &self.state.model
    }

    /// The model to continue a resumed session with: the last one it used,
    /// unless its provider is no longer configured, in which case `fallback`
    /// stays in effect.
    pub(crate) fn resumed_model(&self, config: &Config, fallback: &str) -> String {
        let model = &self.state.model;
        if model.is_empty() || !crate::model::is_resolvable(config, model) {
            return fallback.to_string();
        }
        model.clone()
    }

    /// Records a mid-session model change so later resumes adopt it.
    pub(crate) fn append_model_switch(&mut self, model: &str) -> Result<(), Error> {
        if model == self.state.model {
            return Ok(());
        }
        self.record(SessionEvent::ModelSwitch {
            model: model.to_string(),
        })
    }

    /// The session's own reasoning choice, if it has one.
    ///
    /// `None` means resume should use the saved config default. `Some(None)`
    /// is an explicit provider default. `Some(Some(level))` is an explicit
    /// effort.
    pub(crate) fn recorded_reasoning(&self) -> Option<Option<String>> {
        self.state.reasoning_effort.clone()
    }

    /// Records a reasoning choice so a later resume keeps it instead of the
    /// saved config default. Repeating the current choice writes nothing.
    pub(crate) fn append_reasoning_effort(&mut self, effort: Option<String>) -> Result<(), Error> {
        if self.state.reasoning_effort.as_ref() == Some(&effort) {
            return Ok(());
        }
        self.record(SessionEvent::ReasoningEffort {
            effort,
            inherit: false,
        })
    }

    /// Drops a session reasoning choice so later resumes follow the saved
    /// config default. A session that already inherits writes nothing.
    pub(crate) fn clear_reasoning_effort(&mut self) -> Result<(), Error> {
        if self.state.reasoning_effort.is_none() {
            return Ok(());
        }
        self.record(SessionEvent::ReasoningEffort {
            effort: None,
            inherit: true,
        })
    }

    pub(crate) fn append_message(&mut self, message: Message) -> Result<(), Error> {
        self.record(SessionEvent::Message { message })
    }

    /// Replaces `range` of the history with `summary`. `provider_data` is a
    /// provider-native replacement history from remote compaction.
    pub(crate) fn append_compaction(
        &mut self,
        summary: &str,
        range: std::ops::Range<usize>,
        provider_data: Vec<serde_json::Value>,
        provider_data_model: Option<String>,
    ) -> Result<(), Error> {
        self.record(SessionEvent::Compaction {
            plan_revision: None,
            summary: summary.to_string(),
            start: range.start,
            replaced: range.len(),
            provider_data,
            provider_data_model,
        })
    }

    /// Drops the last `dropped` messages and restores `plan_state`.
    pub(crate) fn append_undo(
        &mut self,
        dropped: usize,
        clear_goal: bool,
        plan_state: Option<PlanState>,
    ) -> Result<(), Error> {
        self.record(SessionEvent::Undo {
            dropped,
            clear_goal,
            plan_state,
        })
    }

    pub(crate) fn append_goal_start(&mut self, goal: &str, message: Message) -> Result<(), Error> {
        self.record(SessionEvent::GoalStart {
            goal: goal.to_string(),
            message,
        })
    }

    pub(crate) fn append_goal_complete(&mut self, message: Message) -> Result<(), Error> {
        self.record(SessionEvent::GoalComplete { message })
    }

    pub(crate) fn append_goal_cancel(&mut self) -> Result<(), Error> {
        self.record(SessionEvent::GoalCancel)
    }

    pub(crate) fn active_goal(&self) -> Option<&str> {
        self.state.active_goal.as_deref()
    }

    pub(crate) fn append_plan_start(
        &mut self,
        objective: &str,
        message: Message,
    ) -> Result<(), Error> {
        self.record(SessionEvent::PlanStart {
            objective: objective.to_string(),
            message,
            previous: self.state.active_plan.clone(),
        })
    }

    pub(crate) fn append_plan_questions_asked(&mut self) -> Result<(), Error> {
        let Some(PlanState::Draft {
            questions_asked, ..
        }) = self.state.active_plan.as_ref()
        else {
            return Err(Error::Protocol(
                "cannot record planning questions without a draft plan".into(),
            ));
        };
        if *questions_asked {
            return Ok(());
        }
        self.record(SessionEvent::PlanQuestionsAsked)
    }

    pub(crate) fn append_plan_implemented(&mut self, message: Message) -> Result<(), Error> {
        self.record(SessionEvent::PlanImplemented { message })
    }

    pub(crate) fn append_plan_cancel(&mut self) -> Result<(), Error> {
        self.record(SessionEvent::PlanCancel)
    }

    pub(crate) fn active_plan(&self) -> Option<&PlanState> {
        self.state.active_plan.as_ref()
    }

    pub(crate) fn plan_before_turn(&self) -> Option<&PlanState> {
        self.state.plan_before_turn.as_ref()
    }

    pub(crate) fn usage(&self) -> UsageSummary {
        self.state.usage
    }

    pub(crate) fn append_usage(&mut self, usage: TokenUsage) -> Result<(), Error> {
        self.record(SessionEvent::Usage { usage })
    }

    /// Removes a session log from `dir`. Missing files are treated as success.
    pub(crate) fn delete(dir: &Path, id: &str) -> Result<(), Error> {
        validate_id(id)?;
        let path = dir.join(format!("{id}.jsonl"));
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::Io(error)),
        }
        match fs::remove_dir_all(dir.join(id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::Io(error)),
        }
    }

    /// Writes `event` to the log, then applies it. A failed write changes
    /// nothing.
    fn record(&mut self, event: SessionEvent) -> Result<(), Error> {
        self.write(&event)?;
        self.state.apply(event);
        Ok(())
    }

    fn write(&mut self, event: &SessionEvent) -> Result<(), Error> {
        #[cfg(test)]
        let fail_write = if let Some(remaining) = &mut self.fail_after_appends {
            if *remaining == 0 {
                self.fail_after_appends = None;
                true
            } else {
                *remaining -= 1;
                false
            }
        } else {
            false
        };
        let Some(storage) = &mut self.storage else {
            #[cfg(test)]
            if fail_write {
                return Err(std::io::Error::other("injected session append failure").into());
            }
            return Ok(());
        };
        if let Some(start) = storage.rollback_to {
            storage.file.set_len(start)?;
            storage.rollback_to = None;
        }
        let mut line = serde_json::to_string(event)?;
        line.push('\n');
        let start = storage.file.metadata()?.len();
        storage.rollback_to = Some(start);
        let written = (|| {
            #[cfg(test)]
            if fail_write {
                storage.file.write_all(&line.as_bytes()[..line.len() / 2])?;
                return Err(std::io::Error::other(
                    "injected partial session append failure",
                ));
            }
            storage.file.write_all(line.as_bytes())?;
            storage.file.sync_data()
        })();
        if let Err(error) = written {
            storage.file.set_len(start)?;
            storage.rollback_to = None;
            return Err(error.into());
        }
        storage.rollback_to = None;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn fail_append_after(&mut self, successful_appends: usize) {
        self.fail_after_appends = Some(successful_appends);
    }
}

fn validate_id(id: &str) -> Result<(), Error> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(Error::Invalid(format!("invalid session id '{id}'")));
    }
    Ok(())
}

struct Replayed {
    state: SessionState,
    /// Lines after the header that could not be decoded.
    skipped_lines: usize,
    /// Start of an unterminated, undecodable final line: an append that was
    /// cut short by a crash.
    torn_tail: Option<u64>,
}

/// Rebuilds a session's state from its log.
fn replay(path: &Path) -> Result<Replayed, Error> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut replayed = Replayed {
        state: SessionState::default(),
        skipped_lines: 0,
        torn_tail: None,
    };
    let mut has_meta = false;
    let mut offset = 0u64;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        let start = offset;
        offset = offset.saturating_add(read as u64);
        let terminated = line.last() == Some(&b'\n');
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let event = serde_json::from_slice::<SessionEvent>(&line);
        if !has_meta {
            let event = event?;
            if !matches!(event, SessionEvent::Meta { .. }) {
                return Err(Error::Protocol(
                    "session log does not start with metadata".into(),
                ));
            }
            has_meta = true;
            replayed.state.apply(event);
            continue;
        }
        // Tolerate unknown or corrupt lines after the required metadata
        // rather than losing the rest of the session, but count them so the
        // user learns the history may be incomplete.
        match event {
            Ok(SessionEvent::Meta { .. }) => {}
            Ok(event) => replayed.state.apply(event),
            Err(_) if !terminated => replayed.torn_tail = Some(start),
            Err(_) => replayed.skipped_lines += 1,
        }
    }
    if !has_meta {
        return Err(Error::Protocol("session log is missing metadata".into()));
    }
    Ok(replayed)
}

pub struct SessionInfo {
    pub id: String,
    pub modified: SystemTime,
    /// First line of the first user message, for pickers.
    pub preview: String,
    /// Last model used in the session: created with, then any `/model` switch.
    pub model: String,
}

/// Whether the session log contains at least one message event.
pub fn has_message(dir: &Path, id: &str) -> bool {
    read_header(&dir.join(format!("{id}.jsonl"))).has_message
}

/// Lists sessions, most recently modified first.
pub fn list(dir: &Path) -> Result<Vec<SessionInfo>, Error> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Io(e)),
    };
    let mut infos: Vec<SessionInfo> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        let header = index::header(&path);
        // Meta-only logs (opened then quit with no turn) are not resumable.
        if !header.has_message {
            continue;
        }
        infos.push(SessionInfo {
            id: id.to_string(),
            modified,
            preview: header.preview,
            model: header.model,
        });
    }
    infos.sort_by_key(|info| std::cmp::Reverse(info.modified));
    Ok(infos)
}

/// Header metadata and preview extracted from a session log in one pass.
#[derive(Clone)]
struct SessionHeader {
    model: String,
    preview: String,
    has_message: bool,
}

/// Cheap guard so listing many large logs stays fast: only lines that could
/// carry a model switch are parsed once the preview is known. The scan still
/// reads the whole file so a late switch is not missed, but after the preview
/// it reuses one byte buffer and skips serde unless the marker bytes match.
const MODEL_SWITCH_MARKER: &str = "\"type\":\"model_switch\"";

fn read_header(path: &Path) -> SessionHeader {
    let mut header = SessionHeader {
        model: String::new(),
        preview: String::new(),
        has_message: false,
    };
    let Ok(file) = File::open(path) else {
        return header;
    };
    let mut reader = BufReader::new(file);
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        // After the preview is found, only later model switches matter.
        if !header.preview.is_empty() {
            if std::str::from_utf8(&buf).is_ok_and(|line| line.contains(MODEL_SWITCH_MARKER))
                && let Ok(SessionEvent::ModelSwitch { model }) = serde_json::from_slice(&buf)
            {
                header.model = model;
            }
            continue;
        }
        match serde_json::from_slice::<SessionEvent>(&buf) {
            Ok(SessionEvent::Meta { model, .. }) => {
                header.model = model;
            }
            Ok(SessionEvent::ModelSwitch { model }) => header.model = model,
            Ok(SessionEvent::Message { message }) => {
                header.has_message = true;
                if message.role == Role::User {
                    let first = message.content.lines().next().unwrap_or("");
                    header.preview = crate::error::truncate(first, 60);
                }
            }
            Ok(
                SessionEvent::GoalStart { message, .. } | SessionEvent::PlanStart { message, .. },
            ) => {
                header.has_message = true;
                if message.role == Role::User {
                    let first = message.content.lines().next().unwrap_or("");
                    header.preview = crate::error::truncate(first, 60);
                }
            }
            Ok(SessionEvent::GoalComplete { .. }) => {
                header.has_message = true;
            }
            _ => {}
        }
    }
    header
}

/// Formats a unix timestamp as `YYYYMMDD-HHMMSS` (UTC) without a date crate.
/// Civil-date conversion after Howard Hinnant's `civil_from_days`.
fn format_timestamp(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let secs_of_day = unix_secs % 86_400;
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        y,
        m,
        d,
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::SubagentResult;

    #[test]
    fn partial_append_rolls_back_before_retry() -> Result<(), Error> {
        let dir = temp_root("partial-append");
        let mut session = Session::create(&dir, &dir, "test")?;
        let path = dir.join(format!("{}.jsonl", session.id));
        let before = fs::read(&path)?;
        session.fail_append_after(0);
        assert!(session.append_message(Message::user("failed")).is_err());
        assert_eq!(fs::read(&path)?, before);
        session.append_message(Message::user("saved"))?;
        let messages = Session::open(&dir, &session.id)?.messages().to_vec();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "saved");
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn append_after_a_crash_tail_keeps_the_new_event() -> Result<(), Error> {
        let dir = temp_root("crash-tail");
        let session = Session::create(&dir, &dir, "test")?;
        let path = dir.join(format!("{}.jsonl", session.id));
        OpenOptions::new()
            .append(true)
            .open(&path)?
            .write_all(b"{\"type\":\"message\",\"message\":")?;
        let mut resumed = Session::open(&dir, &session.id)?;
        resumed.append_message(Message::user("after crash"))?;
        let messages = Session::open(&dir, &session.id)?.messages().to_vec();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "after crash");
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn timestamp_formats_known_date() {
        // 2026-08-20 09:33:01 UTC
        assert_eq!(format_timestamp(1_787_218_381), "20260820-093301");
        assert_eq!(format_timestamp(0), "19700101-000000");
    }

    #[test]
    fn session_roundtrip_with_compaction() -> Result<(), Error> {
        let dir = crate::test_support::unique_temp_dir("test");
        let mut s = Session::create(&dir, Path::new("/projects/demo"), "test-model")?;
        let id = s.id.clone();
        s.append_message(Message::user("one"))?;
        s.append_message(Message::assistant("two".into(), vec![]))?;
        s.append_message(Message::user("three"))?;
        s.append_compaction("summary of one+two", 0..2, Vec::new(), None)?;
        s.append_message(Message::user("four"))?;
        drop(s);

        let messages = Session::open(&dir, &id)?.messages().to_vec();
        assert_eq!(messages.len(), 3);
        assert!(messages[0].content.contains("summary of one+two"));
        assert_eq!(messages[1].content, "three");
        assert_eq!(messages[2].content, "four");
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn session_replays_usage_and_compaction_cache_resets() -> Result<(), Error> {
        let dir = temp_root("usage");
        let mut session = Session::create(&dir, Path::new("/projects/demo"), "test-model")?;
        let id = session.id.clone();
        session.append_message(Message::user("one"))?;
        session.append_usage(TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            cached_input_tokens: 75,
            cache_write_input_tokens: 10,
            cache_details_reported: true,
        })?;
        session.append_compaction("summary", 0..1, Vec::new(), None)?;
        drop(session);

        let session = Session::open(&dir, &id)?;
        assert_eq!(session.usage().requests, 1);
        assert_eq!(session.usage().tokens.cached_input_tokens, 75);
        assert_eq!(session.usage().cache_hit_percent(), 75);
        assert_eq!(session.usage().cache_resets, 1);
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn session_roundtrip_with_middle_compaction() -> Result<(), Error> {
        let dir = crate::test_support::unique_temp_dir("middle-compaction-session");
        let mut session = Session::create(&dir, Path::new("/projects/demo"), "test-model")?;
        let id = session.id.clone();
        for content in ["before", "goal", "progress one", "progress two", "tail"] {
            session.append_message(Message::user(content))?;
        }
        session.append_compaction("progress summary", 2..4, Vec::new(), None)?;
        drop(session);

        let messages = Session::open(&dir, &id)?.messages().to_vec();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].content, "before");
        assert_eq!(messages[1].content, "goal");
        assert!(messages[2].content.contains("progress summary"));
        assert_eq!(messages[3].content, "tail");
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn session_roundtrip_preserves_remote_compaction_history() -> Result<(), Error> {
        let dir = temp_root("remote-compaction");
        let mut session = Session::create(&dir, Path::new("/projects/demo"), "gpt-test")?;
        let id = session.id.clone();
        session.append_message(Message::user("one"))?;
        let provider_data = [serde_json::json!({
            "type": "compaction",
            "encrypted_content": "opaque"
        })];
        session.append_compaction(
            "portable summary",
            0..1,
            provider_data.to_vec(),
            Some("gpt-test".into()),
        )?;
        drop(session);

        let messages = Session::open(&dir, &id)?.messages().to_vec();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].content.contains("portable summary"));
        assert_eq!(messages[0].provider_data, provider_data);
        assert_eq!(messages[0].provider_data_model.as_deref(), Some("gpt-test"));
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn session_roundtrip_with_undo() -> Result<(), Error> {
        let dir = crate::test_support::unique_temp_dir("undo-session");
        let mut session = Session::create(&dir, Path::new("/projects/demo"), "test-model")?;
        let id = session.id.clone();
        session.append_message(Message::user("one"))?;
        session.append_message(Message::assistant("two".into(), vec![]))?;
        session.append_message(Message::user("three"))?;
        session.append_message(Message::assistant("four".into(), vec![]))?;
        session.append_undo(2, false, None)?;
        session.append_message(Message::user("five"))?;
        session.append_message(Message::assistant("six".into(), vec![]))?;
        session.append_undo(2, false, None)?;
        drop(session);

        let messages = Session::open(&dir, &id)?.messages().to_vec();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "one");
        assert_eq!(messages[1].content, "two");
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn session_roundtrip_replays_a_paused_goal() -> Result<(), Error> {
        let dir = crate::test_support::unique_temp_dir("goal-session");
        let mut session = Session::create(&dir, Path::new("/projects/demo"), "test-model")?;
        let id = session.id.clone();
        let start = Message::user("ship the feature")
            .with_control(crate::provider::MessageControl::GoalStart);
        session.append_goal_start("ship the feature", start)?;
        session.append_message(Message::assistant("working".into(), vec![]))?;
        drop(session);

        let session = Session::open(&dir, &id)?;
        let messages = session.messages().to_vec();
        assert_eq!(session.active_goal(), Some("ship the feature"));
        assert_eq!(messages.len(), 2);
        assert!(messages[0].is_goal_start());
        assert_eq!(messages[0].content, "ship the feature");
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn session_goal_complete_and_cancel_clear_the_replayed_goal() -> Result<(), Error> {
        let dir = crate::test_support::unique_temp_dir("goal-clear");
        let mut session = Session::create(&dir, Path::new("/projects/demo"), "test-model")?;
        let id = session.id.clone();
        let start =
            Message::user("done later").with_control(crate::provider::MessageControl::GoalStart);
        session.append_goal_start("done later", start)?;
        session.append_goal_complete(Message::assistant("finished".into(), vec![]))?;
        drop(session);

        let session = Session::open(&dir, &id)?;
        let messages = session.messages().to_vec();
        assert_eq!(session.active_goal(), None);
        assert_eq!(
            messages.last().map(|message| message.content.as_str()),
            Some("finished")
        );

        let mut session = Session::create(&dir, Path::new("/projects/demo"), "test-model")?;
        let canceled_id = session.id.clone();
        let start =
            Message::user("cancel me").with_control(crate::provider::MessageControl::GoalStart);
        session.append_goal_start("cancel me", start)?;
        session.append_goal_cancel()?;
        drop(session);

        let session = Session::open(&dir, &canceled_id)?;
        let messages = session.messages().to_vec();
        assert_eq!(session.active_goal(), None);
        assert_eq!(messages.len(), 1);
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn session_replays_and_undoes_plan_transitions() -> Result<(), Error> {
        let dir = temp_root("plan-state");
        let mut session = Session::create(&dir, Path::new("/projects/demo"), "test-model")?;
        let id = session.id.clone();
        let start = Message::user("add planning mode");
        session.append_plan_start("add planning mode", start)?;
        let ready = Message::assistant("# Plan".into(), vec![]);
        session.append_plan_ready("# Plan", ready)?;
        session.append_message(Message::user("implement it"))?;
        let before_implementation = session.plan_before_turn().cloned();
        session.append_plan_implemented(Message::assistant("done".into(), vec![]))?;
        assert_eq!(session.active_plan(), None);
        session.append_undo(2, false, before_implementation)?;
        drop(session);

        let mut session = Session::open(&dir, &id)?;
        let messages = session.messages().to_vec();
        assert_eq!(messages.len(), 2);
        assert_eq!(
            session.active_plan(),
            Some(&PlanState::Ready {
                plan: "# Plan".into(),
                revision: 1,
            })
        );
        session.append_plan_cancel()?;
        drop(session);
        let session = Session::open(&dir, &id)?;
        assert_eq!(session.active_plan(), None);
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn plan_start_is_resumable_and_question_state_replays() -> Result<(), Error> {
        let dir = temp_root("plan-resume");
        let mut session = Session::create(&dir, Path::new("/projects/demo"), "test-model")?;
        let id = session.id.clone();
        let start = Message::user("add planning mode");
        session.append_plan_start("add planning mode", start.clone())?;

        let listed = list(&dir)?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].preview, "add planning mode");

        session.append_plan_questions_asked()?;
        drop(session);
        let session = Session::open(&dir, &id)?;
        let messages = session.messages().to_vec();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, start.content);
        assert!(
            session
                .active_plan()
                .is_some_and(PlanState::questions_asked)
        );
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn session_undo_can_clear_an_active_goal() -> Result<(), Error> {
        let dir = crate::test_support::unique_temp_dir("goal-undo");
        let mut session = Session::create(&dir, Path::new("/projects/demo"), "test-model")?;
        let id = session.id.clone();
        let start = Message::user("goal").with_control(crate::provider::MessageControl::GoalStart);
        session.append_goal_start("goal", start)?;
        session.append_message(Message::assistant("partial".into(), vec![]))?;
        session.append_undo(2, true, None)?;
        drop(session);

        let session = Session::open(&dir, &id)?;
        let messages = session.messages().to_vec();
        assert_eq!(session.active_goal(), None);
        assert!(messages.is_empty());
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn session_ids_do_not_escape_session_directory() {
        let dir = crate::test_support::unique_temp_dir("session-id-test");
        assert!(Session::open(&dir, "../../outside").is_err());
    }

    #[test]
    fn delivered_subagent_results_replay_with_metadata() -> Result<(), Error> {
        let dir = crate::test_support::unique_temp_dir("subagent-session-test");
        let mut session = Session::create(&dir, Path::new("/projects/demo"), "test-model")?;
        let id = session.id.clone();
        session.append_message(Message::subagent_results(vec![SubagentResult {
            id: "sa-1".into(),
            name: "review".into(),
            status: "completed".into(),
            run_number: 1,
            content: "result".into(),
        }]))?;
        drop(session);

        let messages = Session::open(&dir, &id)?.messages().to_vec();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].subagent_results[0].id, "sa-1");
        assert_eq!(messages[0].subagent_results[0].content, "result");
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    fn write_session(dir: &Path, id: &str, text: &str) -> Result<(), Error> {
        let meta = serde_json::to_string(&SessionEvent::Meta {
            id: id.to_string(),
            created_unix: 1,
            cwd: "/projects/test".into(),
            model: "test-model".into(),
        })?;
        let message = serde_json::to_string(&SessionEvent::Message {
            message: Message::user(text),
        })?;
        fs::write(
            dir.join(format!("{id}.jsonl")),
            format!("{meta}\n{message}\n"),
        )?;
        Ok(())
    }

    fn temp_root(name: &str) -> std::path::PathBuf {
        crate::test_support::unique_temp_dir(&format!("session-{name}"))
    }

    #[test]
    fn meta_records_cwd_and_model_and_lists_them() -> Result<(), Error> {
        let root = temp_root("meta");
        let dir = root.join("sessions");
        let mut session = Session::create(&dir, Path::new("/projects/yawl"), "glm-5.3")?;
        let id = session.id.clone();
        session.append_message(Message::user("hello there"))?;
        drop(session);

        let infos = list(&dir)?;
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].id, id);
        let header = fs::read_to_string(dir.join(format!("{id}.jsonl")))?;
        assert!(header.contains(r#""cwd":"/projects/yawl""#));
        assert_eq!(infos[0].model, "glm-5.3");
        assert_eq!(infos[0].preview, "hello there");
        let _ = fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn model_switches_survive_replay_and_listing() -> Result<(), Error> {
        let root = temp_root("model-switch");
        let dir = root.join("sessions");
        let mut session = Session::create(&dir, Path::new("/projects/yawl"), "first-model")?;
        let id = session.id.clone();
        session.append_message(Message::user("hello there"))?;
        assert_eq!(session.model(), "first-model");
        // A switch to the current model writes nothing.
        session.append_model_switch("first-model")?;
        session.append_model_switch("second-model")?;
        session.append_message(Message::assistant("answer".into(), vec![]))?;
        session.append_model_switch("third-model")?;
        assert_eq!(session.model(), "third-model");
        drop(session);

        let session = Session::open(&dir, &id)?;
        let messages = session.messages().to_vec();
        assert_eq!(session.model(), "third-model");
        assert_eq!(messages.len(), 2);

        let infos = list(&dir)?;
        assert_eq!(infos[0].model, "third-model");
        assert_eq!(infos[0].preview, "hello there");
        let _ = fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn resumed_model_prefers_the_last_switch_when_it_resolves() -> Result<(), Error> {
        let dir = temp_root("resumed-model");
        let config = Config::test_default();
        let mut session = Session::create(&dir, Path::new("/projects/demo"), "first-model")?;
        session.append_message(Message::user("hi"))?;
        assert_eq!(session.resumed_model(&config, "configured"), "first-model");

        session.append_model_switch("openai-codex:gpt-5.6-sol")?;
        assert_eq!(
            session.resumed_model(&config, "configured"),
            "openai-codex:gpt-5.6-sol"
        );

        // An unknown custom provider cannot route, so the fallback stays.
        session.append_model_switch("removed-provider:some-model")?;
        assert_eq!(session.resumed_model(&config, "configured"), "configured");

        let reopened = Session::open(&dir, &session.id)?;
        assert_eq!(reopened.model(), "removed-provider:some-model");
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn reasoning_choice_survives_replay_and_explicit_default_is_distinct() -> Result<(), Error> {
        let root = temp_root("reasoning");
        let dir = root.join("sessions");
        let mut session = Session::create(&dir, Path::new("/projects/yawl"), "model")?;
        let id = session.id.clone();
        assert_eq!(session.recorded_reasoning(), None);
        session.append_reasoning_effort(Some("medium".into()))?;
        session.append_reasoning_effort(Some("medium".into()))?;
        session.append_message(Message::user("hello"))?;
        drop(session);

        let mut session = Session::open(&dir, &id)?;
        assert_eq!(session.recorded_reasoning(), Some(Some("medium".into())));
        session.append_reasoning_effort(None)?;
        drop(session);

        let mut session = Session::open(&dir, &id)?;
        assert_eq!(session.recorded_reasoning(), Some(None));
        session.clear_reasoning_effort()?;
        drop(session);

        let session = Session::open(&dir, &id)?;
        assert_eq!(session.recorded_reasoning(), None);
        let _ = fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn list_skips_meta_only_sessions() -> Result<(), Error> {
        let root = temp_root("empty-list");
        let dir = root.join("sessions");
        let empty = Session::create(&dir, Path::new("/projects/demo"), "test-model")?;
        drop(empty);
        let mut kept = Session::create(&dir, Path::new("/projects/demo"), "test-model")?;
        let kept_id = kept.id.clone();
        kept.append_message(Message::user("real turn"))?;
        drop(kept);

        let infos = list(&dir)?;
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].id, kept_id);
        let _ = fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn delete_removes_session_file() -> Result<(), Error> {
        let root = temp_root("delete");
        let dir = root.join("sessions");
        let mut session = Session::create(&dir, Path::new("/projects/demo"), "test-model")?;
        let id = session.id.clone();
        session.append_message(Message::user("bye"))?;
        drop(session);

        Session::delete(&dir, &id)?;
        assert!(!dir.join(format!("{id}.jsonl")).exists());
        Session::delete(&dir, &id)?;
        let _ = fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn session_headers_require_cwd_and_model() -> Result<(), Error> {
        let root = temp_root("required-meta");
        fs::create_dir_all(&root)?;
        let id = "20260101-000000-0001";
        fs::write(
            root.join(format!("{id}.jsonl")),
            format!(r#"{{"type":"meta","id":"{id}","created_unix":1}}"#),
        )?;

        let error = Session::open(&root, id).unwrap_err();
        assert!(matches!(error, Error::Protocol(_)));
        let _ = fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn open_searching_skips_missing_files_and_opens_a_unique_match() -> Result<(), Error> {
        let root = temp_root("searching");
        let missing = root.join("missing");
        let found = root.join("found");
        fs::create_dir_all(&found)?;
        write_session(&found, "20260101-000000-0001", "found elsewhere")?;

        let session = Session::open_searching(&[missing, found], "20260101-000000-0001")?;
        assert_eq!(session.messages()[0].content, "found elsewhere");

        let error = Session::open_searching(std::slice::from_ref(&root), "20990101-000000-0000")
            .unwrap_err();
        assert!(error.to_string().contains("not found"));
        let _ = fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn open_searching_rejects_duplicate_ids() -> Result<(), Error> {
        let root = temp_root("ambiguous");
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir_all(&first)?;
        fs::create_dir_all(&second)?;
        let id = "20260101-000000-0001";
        write_session(&first, id, "from first")?;
        write_session(&second, id, "from second")?;

        let error = Session::open_searching(&[first, second], id).unwrap_err();
        assert!(error.to_string().contains("ambiguous"));
        let _ = fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn open_searching_propagates_replay_errors() -> Result<(), Error> {
        let root = temp_root("search-error");
        let broken = root.join("broken");
        let valid = root.join("valid");
        fs::create_dir_all(&broken)?;
        fs::create_dir_all(&valid)?;
        let id = "20260101-000000-0001";
        fs::write(broken.join(format!("{id}.jsonl")), [0xff])?;
        write_session(&valid, id, "must not mask the error")?;

        // A log whose header cannot be decoded fails loudly instead of
        // falling through to the other directory.
        let error = Session::open_searching(&[broken, valid], id).unwrap_err();
        assert!(matches!(error, Error::Protocol(_)), "{error}");
        let _ = fs::remove_dir_all(&root);
        Ok(())
    }

    fn subagent_delivery() -> Message {
        Message::subagent_results(vec![SubagentResult {
            id: "sa-1".into(),
            name: "scout".into(),
            status: "completed".into(),
            run_number: 1,
            content: "found it".into(),
        }])
    }

    #[test]
    fn replay_rebuilds_exactly_the_live_state() -> Result<(), Error> {
        use crate::provider::MessageControl;
        let dir = temp_root("live-equals-replay");
        let mut session = Session::create(&dir, Path::new("/projects/demo"), "model-a")?;
        session.append_message(Message::user("first"))?;
        session.append_message(Message::assistant("answer".into(), vec![]))?;
        session.append_model_switch("model-b")?;
        session.append_reasoning_effort(Some("high".into()))?;
        session.append_usage(TokenUsage {
            input_tokens: 10,
            output_tokens: 2,
            ..TokenUsage::default()
        })?;
        session.record_context(ContextUsage {
            tokens: 12,
            messages: 2,
            model: "model-b".into(),
            overhead: 1,
        })?;
        session.append_plan_start(
            "objective",
            Message::user("objective").with_control(MessageControl::PlanStart),
        )?;
        session.append_plan_questions_asked()?;
        session.append_plan_ready("# Plan", Message::assistant("# Plan".into(), vec![]))?;
        session.append_message(
            Message::user("Implement the active plan now.")
                .with_control(MessageControl::PlanImplementationStart),
        )?;
        session.append_plan_handoff(1, "handoff", 2..4)?;
        session.append_goal_start(
            "goal",
            Message::user("goal").with_control(MessageControl::GoalStart),
        )?;
        session.append_message(subagent_delivery())?;
        session.append_compaction("summary", 0..1, Vec::new(), None)?;
        let restored = session.plan_before_turn().cloned();
        session.append_undo(2, true, restored)?;
        session.append_plan_implemented(Message::assistant("done".into(), vec![]))?;
        session.clear_reasoning_effort()?;
        let live = format!("{:?}", session.state);
        let id = session.id.clone();
        drop(session);

        let replayed = Session::open(&dir, &id)?;
        assert_eq!(format!("{:?}", replayed.state), live);
        assert_eq!(replayed.replay_warning(), None);
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn subagent_deliveries_and_goal_starts_snapshot_the_plan_correctly() -> Result<(), Error> {
        use crate::provider::MessageControl;
        let ready = || PlanState::Ready {
            plan: "# Plan".into(),
            revision: 1,
        };
        let mut session = Session::memory("memory".into(), "test");
        session.append_plan_start(
            "objective",
            Message::user("objective").with_control(MessageControl::PlanStart),
        )?;
        session.append_plan_ready("# Plan", Message::assistant("# Plan".into(), vec![]))?;
        // Starting a goal begins a turn while the plan is ready.
        session.append_goal_start(
            "goal",
            Message::user("goal").with_control(MessageControl::GoalStart),
        )?;
        assert_eq!(session.plan_before_turn(), Some(&ready()));
        session.append_plan_implemented(Message::assistant("done".into(), vec![]))?;
        // A late subagent delivery continues the turn; it must not move the
        // point `/undo` restores to.
        session.append_message(subagent_delivery())?;
        assert_eq!(session.plan_before_turn(), Some(&ready()));
        Ok(())
    }

    #[test]
    fn compaction_splices_a_middle_range_and_keeps_the_undo_anchor() -> Result<(), Error> {
        let mut session = Session::memory("memory".into(), "test");
        for index in 0..24 {
            session.append_message(Message::user(format!("message {index}")))?;
        }
        session.append_compaction("goal progress", 4..14, Vec::new(), None)?;
        let messages = session.messages();
        assert_eq!(messages[3].content, "message 3");
        assert!(messages[4].is_summary());
        assert!(messages[4].content.contains("goal progress"));
        assert_eq!(messages[5].content, "message 14");
        assert_eq!(
            messages.last().map(|m| m.content.as_str()),
            Some("message 23")
        );
        assert_eq!(session.usage().cache_resets, 1);
        Ok(())
    }

    #[test]
    fn a_prompt_that_looks_like_a_summary_is_still_a_prompt() {
        let prompt = Message::user("[conversation summary]\nplease review this");
        assert!(!prompt.is_summary());
        assert!(prompt.starts_user_turn());
    }

    #[test]
    fn replay_drops_a_torn_tail_and_reports_unreadable_entries() -> Result<(), Error> {
        let dir = temp_root("torn-tail");
        let mut session = Session::create(&dir, Path::new("/projects/demo"), "test")?;
        let id = session.id.clone();
        session.append_message(Message::user("kept"))?;
        drop(session);
        let path = dir.join(format!("{id}.jsonl"));
        let mut log = OpenOptions::new().append(true).open(&path)?;
        log.write_all(b"{\"type\":\"bogus\"}\n{\"type\":\"message\",\"mess")?;
        drop(log);

        let mut session = Session::open(&dir, &id)?;
        assert_eq!(session.messages().len(), 1);
        let warning = session
            .replay_warning()
            .expect("unreadable entry is reported");
        assert!(warning.contains("1 unreadable log entry"), "{warning}");
        assert!(!fs::read_to_string(&path)?.contains("\"mess\n"));
        session.append_message(Message::user("after crash"))?;
        drop(session);

        let session = Session::open(&dir, &id)?;
        let contents: Vec<_> = session
            .messages()
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(contents, ["kept", "after crash"]);
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn memory_sessions_apply_events_without_a_file() -> Result<(), Error> {
        let mut session = Session::memory("child".into(), "test");
        session.append_message(Message::user("hello"))?;
        session.append_usage(TokenUsage {
            input_tokens: 3,
            ..TokenUsage::default()
        })?;
        assert_eq!(session.messages().len(), 1);
        assert_eq!(session.usage().requests, 1);
        assert_eq!(session.ensure_plan_file()?, None);
        Ok(())
    }
}
