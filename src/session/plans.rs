//! Recoverable Markdown artifacts and durable implementation handoffs.

use super::{PlanState, Session, SessionEvent};
use crate::error::Error;
use crate::provider::Message;
use std::fs;
use std::path::PathBuf;

impl Session {
    /// Where plan revisions are saved; `None` for memory-only sessions.
    fn plans_directory(&self) -> Option<PathBuf> {
        let storage = self.storage.as_ref()?;
        Some(storage.directory.join(&self.id).join("plans"))
    }

    fn save_plan(&self, revision: usize, plan: &str) -> Result<Option<PathBuf>, Error> {
        let Some(directory) = self.plans_directory() else {
            return Ok(None);
        };
        let path = directory.join(format!("{revision}.md"));
        match fs::read_to_string(&path) {
            Ok(saved) if saved == plan => return Ok(Some(path)),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        fs::create_dir_all(directory)?;
        // The log is authoritative. Rename also repairs partial or externally
        // modified artifacts without exposing a partially written plan.
        let temporary = path.with_extension("md.tmp");
        fs::write(&temporary, plan)?;
        fs::rename(&temporary, &path)?;
        Ok(Some(path))
    }

    pub(crate) fn ensure_plan_file(&self) -> Result<Option<PathBuf>, Error> {
        match &self.state.active_plan {
            Some(PlanState::Ready { plan, revision }) => self.save_plan(*revision, plan),
            _ => Ok(None),
        }
    }

    /// Saves the completed plan as the next revision's file, then records
    /// it. Returns the file, or `None` for memory-only sessions.
    pub(crate) fn append_plan_ready(
        &mut self,
        plan: &str,
        message: Message,
    ) -> Result<Option<PathBuf>, Error> {
        let revision = self.state.plan_revisions.len() + 1;
        let path = self.save_plan(revision, plan)?;
        self.record(SessionEvent::PlanReady {
            plan: plan.to_string(),
            message,
        })?;
        Ok(path)
    }

    pub(crate) fn plan_has_handoff(&self, revision: usize) -> bool {
        self.state.plan_handoffs.contains(&revision)
    }

    /// Records that `range` of the history was replaced by the handoff for
    /// plan `revision`. Replay rebuilds it as a handoff, not a summary.
    pub(crate) fn append_plan_handoff(
        &mut self,
        revision: usize,
        handoff: &str,
        range: std::ops::Range<usize>,
    ) -> Result<(), Error> {
        self.record(SessionEvent::Compaction {
            summary: handoff.to_string(),
            start: range.start,
            replaced: range.len(),
            provider_data: Vec::new(),
            provider_data_model: None,
            plan_revision: Some(revision),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            Self(crate::test_support::unique_temp_dir("plan"))
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
        session.append_plan_ready(first, Message::assistant(first.into(), vec![]))?;
        let previous = session.active_plan().cloned();
        let first_path = session.ensure_plan_file()?.unwrap();
        assert!(first_path.is_absolute());
        assert_eq!(fs::read_to_string(&first_path)?, first);
        session.append_plan_handoff(1, "summary and first plan", 0..1)?;
        session.append_plan_ready("# Second", Message::assistant("# Second".into(), vec![]))?;
        let second_path = session.ensure_plan_file()?.unwrap();
        assert_ne!(first_path, second_path);
        assert_eq!(fs::read_to_string(&first_path)?, first);
        assert!(!session.plan_has_handoff(2));
        session.append_undo(1, false, previous)?;
        fs::remove_file(&first_path)?;
        drop(session);
        let session = Session::open(&fixture.0, &id)?;
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
        session.record(SessionEvent::PlanReady {
            plan: "# Legacy".into(),
            message: Message::assistant("# Legacy".into(), vec![]),
        })?;
        let legacy: PlanState = serde_json::from_str(r##"{"status":"ready","plan":"# Legacy"}"##)?;
        session.append_undo(0, false, Some(legacy))?;
        drop(session);
        assert!(!fixture.0.join(&id).exists());
        let session = Session::open(&fixture.0, &id)?;
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
        session.append_plan_ready("# Plan", Message::assistant("# Plan".into(), vec![]))?;
        session.fail_append_after(0);
        assert!(session.append_plan_handoff(1, "handoff", 0..1).is_err());
        assert!(!session.plan_has_handoff(1));
        drop(session);
        let session = Session::open(&fixture.0, &id)?;
        let messages = session.messages().to_vec();
        assert!(!session.plan_has_handoff(1));
        assert_eq!(messages[0].content, "# Plan");
        Ok(())
    }
}
