//! Append-only JSONL session persistence, one `<id>.jsonl` file per session
//! in a caller-chosen directory. The config facade resolves the per-project
//! layout under `~/.yawl/sessions/`; this module stays layout-agnostic.
//!
//! The file keeps the full original history forever; compaction is recorded
//! as an event and applied at replay time, so the in-memory conversation is
//! rebuilt by replaying the log.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::provider::{Message, Role};

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SessionEvent {
    Meta {
        id: String,
        created_unix: u64,
        /// Canonical working directory the session belongs to.
        cwd: String,
        /// Model active when the session started.
        model: String,
    },
    Message {
        message: Message,
    },
    /// The first `replaced` messages of the conversation (at that point in
    /// the replay) were folded into `summary`.
    Compaction {
        summary: String,
        replaced: usize,
    },
}

#[derive(Debug)]
pub struct Session {
    pub id: String,
    file: File,
}

impl Session {
    /// Creates a new session with a timestamp-derived id, recording the
    /// working directory and model in the header event.
    pub fn create(dir: &Path, cwd: &Path, model: &str) -> Result<Session, Error> {
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
            .ok_or_else(|| Error::Config("could not allocate a session id".into()))?;
        let mut session = Session {
            id: id.clone(),
            file,
        };
        session.append(&SessionEvent::Meta {
            id,
            created_unix: now.as_secs(),
            cwd: cwd.to_string_lossy().into_owned(),
            model: model.to_string(),
        })?;
        Ok(session)
    }

    /// Opens an existing session by id and replays its messages.
    pub fn open(dir: &Path, id: &str) -> Result<(Session, Vec<Message>), Error> {
        validate_id(id)?;
        let path = dir.join(format!("{id}.jsonl"));
        let messages = replay(&path)?;
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok((
            Session {
                id: id.to_string(),
                file,
            },
            messages,
        ))
    }

    /// Opens the most recently modified session, if any.
    pub fn open_latest(dir: &Path) -> Result<Option<(Session, Vec<Message>)>, Error> {
        match list(dir)?.first() {
            Some(info) => Ok(Some(Session::open(dir, &info.id)?)),
            None => Ok(None),
        }
    }

    /// Opens the unique session matching `id` across project directories.
    /// Missing files are skipped. Replay, permission, and other I/O errors
    /// are returned, and duplicate ids are rejected as ambiguous.
    pub fn open_searching(dirs: &[PathBuf], id: &str) -> Result<(Session, Vec<Message>), Error> {
        validate_id(id)?;
        let mut found: Option<(PathBuf, (Session, Vec<Message>))> = None;
        for dir in dirs {
            match Session::open(dir, id) {
                Ok(session) => {
                    if let Some((found_dir, _)) = &found {
                        return Err(Error::Config(format!(
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
            .ok_or_else(|| Error::Config(format!("session '{id}' not found")))
    }

    pub fn append_message(&mut self, message: &Message) -> Result<(), Error> {
        self.append(&SessionEvent::Message {
            message: message.clone(),
        })
    }

    pub fn append_compaction(&mut self, summary: &str, replaced: usize) -> Result<(), Error> {
        self.append(&SessionEvent::Compaction {
            summary: summary.to_string(),
            replaced,
        })
    }

    fn append(&mut self, event: &SessionEvent) -> Result<(), Error> {
        let mut line = serde_json::to_string(event)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        Ok(())
    }
}

fn validate_id(id: &str) -> Result<(), Error> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(Error::Config(format!("invalid session id '{id}'")));
    }
    Ok(())
}

/// Rebuilds the effective conversation from a session log.
fn replay(path: &Path) -> Result<Vec<Message>, Error> {
    let file = File::open(path)?;
    let mut messages: Vec<Message> = Vec::new();
    let mut has_meta = false;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if !has_meta {
            match serde_json::from_str::<SessionEvent>(&line)? {
                SessionEvent::Meta { .. } => {
                    has_meta = true;
                    continue;
                }
                _ => {
                    return Err(Error::Protocol(
                        "session log does not start with metadata".into(),
                    ));
                }
            }
        }
        // Tolerate unknown/corrupt lines after the required metadata rather
        // than losing the rest of the session.
        let Ok(event) = serde_json::from_str::<SessionEvent>(&line) else {
            continue;
        };
        match event {
            SessionEvent::Meta { .. } => {}
            SessionEvent::Message { message } => messages.push(message),
            SessionEvent::Compaction { summary, replaced } => {
                let replaced = replaced.min(messages.len());
                let tail = messages.split_off(replaced);
                messages = vec![crate::compaction::summary_message(&summary)];
                messages.extend(tail);
            }
        }
    }
    if !has_meta {
        return Err(Error::Protocol("session log is missing metadata".into()));
    }
    Ok(messages)
}

pub struct SessionInfo {
    pub id: String,
    pub modified: SystemTime,
    /// First line of the first user message, for pickers.
    pub preview: String,
    /// Canonical working directory recorded at session creation.
    pub cwd: String,
    /// Model recorded at session creation.
    pub model: String,
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
        let header = read_header(&path);
        infos.push(SessionInfo {
            id: id.to_string(),
            modified,
            preview: header.preview,
            cwd: header.cwd,
            model: header.model,
        });
    }
    infos.sort_by_key(|info| std::cmp::Reverse(info.modified));
    Ok(infos)
}

/// Header metadata and preview extracted from a session log in one pass.
struct SessionHeader {
    cwd: String,
    model: String,
    preview: String,
}

fn read_header(path: &Path) -> SessionHeader {
    let mut header = SessionHeader {
        cwd: String::new(),
        model: String::new(),
        preview: String::new(),
    };
    let Ok(file) = File::open(path) else {
        return header;
    };
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            break;
        };
        match serde_json::from_str::<SessionEvent>(&line) {
            Ok(SessionEvent::Meta { cwd, model, .. }) => {
                header.cwd = cwd;
                header.model = model;
            }
            Ok(SessionEvent::Message { message }) if message.role == Role::User => {
                let first = message.content.lines().next().unwrap_or("");
                header.preview = crate::error::truncate(first, 60);
                return header;
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
    fn timestamp_formats_known_date() {
        // 2026-08-20 09:33:01 UTC
        assert_eq!(format_timestamp(1_787_218_381), "20260820-093301");
        assert_eq!(format_timestamp(0), "19700101-000000");
    }

    #[test]
    fn session_roundtrip_with_compaction() -> Result<(), Error> {
        let dir = std::env::temp_dir().join(format!("yawl-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut s = Session::create(&dir, Path::new("/projects/demo"), "test-model")?;
        let id = s.id.clone();
        s.append_message(&Message::user("one"))?;
        s.append_message(&Message::assistant("two".into(), vec![]))?;
        s.append_message(&Message::user("three"))?;
        s.append_compaction("summary of one+two", 2)?;
        s.append_message(&Message::user("four"))?;
        drop(s);

        let (_, messages) = Session::open(&dir, &id)?;
        assert_eq!(messages.len(), 3);
        assert!(messages[0].content.contains("summary of one+two"));
        assert_eq!(messages[1].content, "three");
        assert_eq!(messages[2].content, "four");
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn session_ids_do_not_escape_session_directory() {
        let dir = std::env::temp_dir().join(format!("yawl-session-id-test-{}", std::process::id()));
        assert!(Session::open(&dir, "../../outside").is_err());
    }

    #[test]
    fn delivered_subagent_results_replay_with_metadata() -> Result<(), Error> {
        let dir =
            std::env::temp_dir().join(format!("yawl-subagent-session-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut session = Session::create(&dir, Path::new("/projects/demo"), "test-model")?;
        let id = session.id.clone();
        session.append_message(&Message::subagent_results(vec![SubagentResult {
            id: "sa-1".into(),
            name: "review".into(),
            status: "completed".into(),
            run_number: 1,
            content: "result".into(),
        }]))?;
        drop(session);

        let (_, messages) = Session::open(&dir, &id)?;
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
        std::env::temp_dir().join(format!(
            "yawl-session-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    #[test]
    fn meta_records_cwd_and_model_and_lists_them() -> Result<(), Error> {
        let root = temp_root("meta");
        let dir = root.join("sessions");
        let mut session = Session::create(&dir, Path::new("/projects/yawl"), "glm-5.3")?;
        let id = session.id.clone();
        session.append_message(&Message::user("hello there"))?;
        drop(session);

        let infos = list(&dir)?;
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].id, id);
        assert_eq!(infos[0].cwd, "/projects/yawl");
        assert_eq!(infos[0].model, "glm-5.3");
        assert_eq!(infos[0].preview, "hello there");
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

        let (_, messages) = Session::open_searching(&[missing, found], "20260101-000000-0001")?;
        assert_eq!(messages[0].content, "found elsewhere");

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
    fn open_searching_propagates_replay_io_errors() -> Result<(), Error> {
        let root = temp_root("search-error");
        let broken = root.join("broken");
        let valid = root.join("valid");
        fs::create_dir_all(&broken)?;
        fs::create_dir_all(&valid)?;
        let id = "20260101-000000-0001";
        fs::write(broken.join(format!("{id}.jsonl")), [0xff])?;
        write_session(&valid, id, "must not mask the error")?;

        let error = Session::open_searching(&[broken, valid], id).unwrap_err();
        assert!(matches!(error, Error::Io(_)));
        let _ = fs::remove_dir_all(&root);
        Ok(())
    }
}
