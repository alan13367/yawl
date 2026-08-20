//! Append-only JSONL session persistence in `~/.yawl/sessions/`.
//!
//! The file keeps the full original history forever; compaction is recorded
//! as an event and applied at replay time, so the in-memory conversation is
//! rebuilt by replaying the log.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
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

pub struct Session {
    pub id: String,
    file: File,
}

impl Session {
    /// Creates a new session with a timestamp-derived id.
    pub fn create(dir: &Path) -> Result<Session, Error> {
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
    let text = fs::read_to_string(path)?;
    let mut messages: Vec<Message> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        // Tolerate unknown/corrupt lines rather than losing the session.
        let Ok(event) = serde_json::from_str::<SessionEvent>(line) else {
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
    Ok(messages)
}

pub struct SessionInfo {
    pub id: String,
    pub modified: SystemTime,
    /// First line of the first user message, for pickers.
    pub preview: String,
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
        infos.push(SessionInfo {
            id: id.to_string(),
            modified,
            preview: preview(&path),
        });
    }
    infos.sort_by_key(|info| std::cmp::Reverse(info.modified));
    Ok(infos)
}

fn preview(path: &Path) -> String {
    let Ok(text) = fs::read_to_string(path) else {
        return String::new();
    };
    for line in text.lines() {
        if let Ok(SessionEvent::Message { message }) = serde_json::from_str(line)
            && message.role == Role::User
        {
            let first = message.content.lines().next().unwrap_or("");
            return crate::error::truncate(first, 60);
        }
    }
    String::new()
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
        let mut s = Session::create(&dir)?;
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
}
