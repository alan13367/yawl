//! Session commands: `/new`, `/resume`, deleting sessions, and `/undo`
//! reports.

use crate::agent::Agent;

use super::super::picker::{Picker, PickerAction, PickerItem};
use super::super::state::ViewState;

pub(in crate::tui) fn open_resume_picker(agent: &Agent, state: &mut ViewState) {
    let cwd = crate::config::working_dir();
    let dirs = agent.config().session_dirs(&cwd);
    let sessions = match crate::session::list(&dirs.project) {
        Ok(sessions) => sessions,
        Err(error) => {
            state.notice(format!("Could not list sessions: {error}"));
            return;
        }
    };
    if sessions.is_empty() {
        state.picker = None;
        state.notice("No saved sessions for this directory.");
        return;
    }
    state.picker = Some(Picker {
        title: "Resume session".into(),
        hint: "↑/↓ move  Enter resume  d delete…  Esc cancel".into(),
        selected: 0,
        items: sessions
            .into_iter()
            .take(100)
            .map(|session| PickerItem {
                label: if session.preview.is_empty() {
                    "Untitled session".into()
                } else {
                    session.preview
                },
                description: format!("{} · {}", session.model, session.id),
                action: PickerAction::ResumeSession(session.id),
            })
            .collect(),
        editing: None,
        filter: None,
        parent: None,
    });
}

pub(in crate::tui) fn notice_undo(state: &mut ViewState, report: crate::agent::UndoReport) {
    if report.dropped == 0 {
        state.notice("Nothing to undo.");
        return;
    }
    let mut text = "Undid the last turn.".to_string();
    if report.reset_head {
        text.push_str(" Reset git HEAD.");
    }
    if !report.restored_files {
        text.push_str(" File changes could not be restored.");
    }
    if let Some(warning) = report.warning {
        text.push(' ');
        text.push_str(&warning);
    }
    state.notice(text);
}

pub(in crate::tui) fn resume(agent: &mut Agent, selector: &str, state: &mut ViewState) {
    if !selector.is_empty() && selector.parse::<usize>().is_err() {
        load_session(agent, selector, state);
        return;
    }

    let cwd = crate::config::working_dir();
    let dirs = agent.config().session_dirs(&cwd);
    let sessions = match crate::session::list(&dirs.project) {
        Ok(sessions) => sessions,
        Err(error) => {
            state.notice(format!("Could not list sessions: {error}"));
            return;
        }
    };
    if selector.is_empty() {
        if sessions.is_empty() {
            state.notice("No saved sessions for this directory.");
            return;
        }
        let mut text = String::from("Saved sessions\n\n");
        for (index, session) in sessions.iter().take(20).enumerate() {
            text.push_str(&format!(
                "{}. `{}`  {}\n",
                index + 1,
                session.id,
                session.preview
            ));
        }
        text.push_str("\nUse `/resume ID` or `/resume NUMBER`.");
        state.notice(text);
        return;
    }
    let id = selector
        .parse::<usize>()
        .ok()
        .and_then(|number| number.checked_sub(1))
        .and_then(|index| sessions.get(index))
        .map_or(selector, |session| session.id.as_str());
    load_session(agent, id, state);
}

pub(in crate::tui) fn new_session(agent: &mut Agent, state: &mut ViewState) {
    match agent.reset() {
        Ok(()) => {
            let queued_inputs = std::mem::take(&mut state.queued_inputs);
            let pending_steers = std::mem::take(&mut state.pending_steers);
            let pending_actions = std::mem::take(&mut state.pending_actions);
            *state = ViewState::from_agent(agent);
            state.queued_inputs = queued_inputs;
            state.pending_steers = pending_steers;
            state.pending_actions = pending_actions;
        }
        Err(error) => state.notice(format!("Could not start a session: {error}")),
    }
}

pub(in crate::tui) fn load_session(agent: &mut Agent, id: &str, state: &mut ViewState) {
    let previous_model = agent.model().to_string();
    match agent.load_session(id) {
        Ok(()) => {
            let queued_inputs = std::mem::take(&mut state.queued_inputs);
            let pending_actions = std::mem::take(&mut state.pending_actions);
            *state = ViewState::from_agent(agent);
            state.queued_inputs = queued_inputs;
            state.pending_actions = pending_actions;
            let model = agent.model().to_string();
            state.notice(if model == previous_model {
                format!("Resumed session {id}.")
            } else {
                format!("Resumed session {id} with {model}.")
            });
            if let Some(warning) = agent.session_warning() {
                state.notice(warning);
            }
        }
        Err(error) => state.notice(format!("Could not resume '{id}': {error}")),
    }
}

pub(in crate::tui) fn delete_session(agent: &mut Agent, id: &str, state: &mut ViewState) {
    let selected = state
        .picker
        .as_ref()
        .map(|picker| picker.selected)
        .unwrap_or(0);
    let deleting_current = agent.session_id() == id;
    match agent.delete_session(id) {
        Ok(()) => {
            if deleting_current {
                let queued_inputs = std::mem::take(&mut state.queued_inputs);
                let pending_actions = std::mem::take(&mut state.pending_actions);
                *state = ViewState::from_agent(agent);
                state.queued_inputs = queued_inputs;
                state.pending_actions = pending_actions;
            }
            let cwd = crate::config::working_dir();
            let dirs = agent.config().session_dirs(&cwd);
            match crate::session::list(&dirs.project) {
                Ok(sessions) if sessions.is_empty() => {
                    state.picker = None;
                    state.notice(format!("Deleted session {id}. No saved sessions left."));
                }
                Ok(_) => {
                    open_resume_picker(agent, state);
                    if let Some(picker) = state.picker.as_mut() {
                        picker.selected = selected.min(picker.items.len().saturating_sub(1));
                    }
                    state.notice(format!("Deleted session {id}."));
                }
                Err(error) => {
                    state.picker = None;
                    state.notice(format!(
                        "Deleted session {id}. Could not refresh the list: {error}"
                    ));
                }
            }
        }
        Err(error) => state.notice(format!("Could not delete '{id}': {error}")),
    }
}
