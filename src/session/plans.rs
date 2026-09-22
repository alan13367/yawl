//! Recoverable Markdown artifacts and durable implementation handoffs.

use super::{PlanState, Session, SessionEvent};
use crate::error::Error;
use crate::provider::Message;
use std::fs;
use std::path::PathBuf;

impl Session {
    fn plan_path(&self, revision: usize) -> PathBuf {
        self.directory
            .join(&self.id)
            .join("plans")
            .join(format!("{revision}.md"))
    }

    fn save_plan(&self, revision: usize, plan: &str) -> Result<PathBuf, Error> {
        let path = self.plan_path(revision);
        match fs::read_to_string(&path) {
            Ok(saved) if saved == plan => return Ok(path),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let directory = self.directory.join(&self.id).join("plans");
        fs::create_dir_all(directory)?;
        // The log is authoritative. Rename also repairs partial or externally
        // modified artifacts without exposing a partially written plan.
        let temporary = path.with_extension("md.tmp");
        fs::write(&temporary, plan)?;
        fs::rename(&temporary, &path)?;
        Ok(path)
    }

    pub(crate) fn ensure_plan_file(&self) -> Result<Option<PathBuf>, Error> {
        match &self.active_plan {
            Some(PlanState::Ready { plan, revision }) => self.save_plan(*revision, plan).map(Some),
            _ => Ok(None),
        }
    }

    pub(crate) fn append_plan_ready(
        &mut self,
        plan: &str,
        message: &Message,
    ) -> Result<PathBuf, Error> {
        let revision = self.plan_revision_count + 1;
        let path = self.save_plan(revision, plan)?;
        self.append(&SessionEvent::PlanReady {
            plan: plan.to_string(),
            message: message.clone(),
        })?;
        self.plan_revision_count = revision;
        self.active_plan = Some(PlanState::Ready {
            plan: plan.to_string(),
            revision,
        });
        Ok(path)
    }

    pub(crate) fn plan_has_handoff(&self, revision: usize) -> bool {
        self.plan_handoffs.contains(&revision)
    }

    pub(crate) fn append_plan_handoff(
        &mut self,
        revision: usize,
        summary: &str,
        replaced: usize,
    ) -> Result<(), Error> {
        self.append(&SessionEvent::Compaction {
            summary: summary.to_string(),
            start: 0,
            replaced,
            provider_data: Vec::new(),
            provider_data_model: None,
            plan_revision: Some(revision),
        })?;
        self.plan_handoffs.insert(revision);
        self.context = None;
        self.usage.record_cache_reset();
        Ok(())
    }
}

/// Undo records written before revision IDs were introduced still contain
/// the plan text, which identifies the latest matching earlier revision.
pub(super) fn restore_revision(
    mut state: Option<PlanState>,
    revisions: &[String],
) -> Option<PlanState> {
    if let Some(PlanState::Ready { plan, revision }) = &mut state
        && *revision == 0
    {
        *revision = revisions
            .iter()
            .rposition(|saved| saved == plan)
            .map_or(0, |i| i + 1);
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            Self(std::env::temp_dir().join(format!("yawl-plan-{}-{nonce}", std::process::id())))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn revisions_survive_undo_resume_and_missing_artifacts() -> Result<(), Error> {
        let fixture = Fixture::new();
        let mut session = Session::create(&fixture.0, &fixture.0, "test")?;
        let id = session.id.clone();
        let first = "# First\n\nPreserve Unicode: café.\n";
        session.append_plan_ready(first, &Message::assistant(first.into(), vec![]))?;
        let previous = session.active_plan.clone();
        let first_path = session.ensure_plan_file()?.unwrap();
        assert!(first_path.is_absolute());
        assert_eq!(fs::read_to_string(&first_path)?, first);
        session.append_plan_handoff(1, "summary and first plan", 1)?;
        session.append_plan_ready("# Second", &Message::assistant("# Second".into(), vec![]))?;
        let second_path = session.ensure_plan_file()?.unwrap();
        assert_ne!(first_path, second_path);
        assert_eq!(fs::read_to_string(&first_path)?, first);
        assert!(!session.plan_has_handoff(2));
        session.append_undo_event_with_plan(1, false, previous)?;
        fs::remove_file(&first_path)?;
        drop(session);
        let (session, _) = Session::open(&fixture.0, &id)?;
        assert_eq!(session.ensure_plan_file()?, Some(first_path.clone()));
        assert_eq!(fs::read_to_string(&first_path)?, first);
        assert!(session.plan_has_handoff(1));
        assert!(!session.plan_has_handoff(2));
        drop(session);
        Session::delete(&fixture.0, &id)?;
        assert!(!first_path.exists());
        assert!(!second_path.exists());
        assert!(!fixture.0.join(format!("{id}.jsonl")).exists());
        Ok(())
    }

    #[test]
    fn legacy_plan_and_undo_receive_revision_ids_lazily() -> Result<(), Error> {
        let fixture = Fixture::new();
        let mut session = Session::create(&fixture.0, &fixture.0, "test")?;
        let id = session.id.clone();
        // Old PlanReady records have the same wire shape and no artifact.
        session.append(&SessionEvent::PlanReady {
            plan: "# Legacy".into(),
            message: Message::assistant("# Legacy".into(), vec![]),
        })?;
        let legacy: PlanState = serde_json::from_str(r##"{"status":"ready","plan":"# Legacy"}"##)?;
        session.append_undo_event_with_plan(0, false, Some(legacy))?;
        drop(session);
        assert!(!fixture.0.join(&id).exists());
        let (session, _) = Session::open(&fixture.0, &id)?;
        assert!(!fixture.0.join(&id).exists());
        let path = session.ensure_plan_file()?.unwrap();
        assert_eq!(path.file_name().unwrap(), "1.md");
        assert_eq!(fs::read_to_string(path)?, "# Legacy");
        Ok(())
    }

    #[test]
    fn failed_handoff_append_does_not_mark_revision_or_replace_replayed_history()
    -> Result<(), Error> {
        let fixture = Fixture::new();
        let mut session = Session::create(&fixture.0, &fixture.0, "test")?;
        let id = session.id.clone();
        session.append_plan_ready("# Plan", &Message::assistant("# Plan".into(), vec![]))?;
        session.fail_append_after(0);
        assert!(session.append_plan_handoff(1, "handoff", 1).is_err());
        assert!(!session.plan_has_handoff(1));
        drop(session);
        let (session, messages) = Session::open(&fixture.0, &id)?;
        assert!(!session.plan_has_handoff(1));
        assert_eq!(messages[0].content, "# Plan");
        Ok(())
    }
}
