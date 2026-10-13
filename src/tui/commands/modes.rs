//! `/init`, `/goal`, and `/plan`: starting, resuming, canceling, and
//! reporting persistent goals and planning workflows.

use crate::agent::Agent;
use crate::provider::TurnInput;

use super::super::picker::{Picker, PickerAction, PickerItem};
use super::super::state::ViewState;

pub(in crate::tui) fn init(argument: &str) -> Result<TurnInput, &'static str> {
    if argument.is_empty() {
        Ok("/init".to_string().into())
    } else {
        Err("Usage: /init")
    }
}

pub(in crate::tui) enum GoalAction {
    None,
    Start,
    Resume,
}

pub(in crate::tui) enum PlanAction {
    None,
    Start,
    Resume,
    Implement,
    /// Revise the ready plan with this (paste-expanded) request.
    Revise(String),
}

/// The request after a `/plan revise` subcommand, trimmed. `None` when
/// `argument` is not that subcommand.
pub(in crate::tui) fn revision_text(argument: &str) -> Option<&str> {
    let rest = argument.strip_prefix("revise")?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim())
}

pub(in crate::tui) fn plan(agent: &mut Agent, argument: &str, state: &mut ViewState) -> PlanAction {
    match argument {
        "" => {
            notice_plan_status(agent, state);
            PlanAction::None
        }
        "cancel" => {
            match agent.cancel_plan() {
                Ok(true) => {
                    state.active_plan = None;
                    state.plan_draft = false;
                    state.notice("Plan canceled.");
                }
                Ok(false) => state.notice("No active plan."),
                Err(error) => state.notice(format!("Could not cancel the plan: {error}")),
            }
            PlanAction::None
        }
        "resume" => match agent.plan_state() {
            Some(crate::session::PlanState::Draft { .. }) => PlanAction::Resume,
            Some(crate::session::PlanState::Ready { .. }) => {
                state.notice(
                    "The plan is already complete. Run /plan revise TEXT or /plan implement.",
                );
                PlanAction::None
            }
            None => {
                state.notice("No interrupted plan to resume.");
                PlanAction::None
            }
        },
        "implement" => match agent.plan_state() {
            Some(crate::session::PlanState::Ready { .. }) => PlanAction::Implement,
            Some(crate::session::PlanState::Draft { .. }) => {
                state.notice(
                    "The plan is still a draft. Finish it, or run /plan resume to continue it.",
                );
                PlanAction::None
            }
            None => {
                state.notice("No active plan.");
                PlanAction::None
            }
        },
        argument if let Some(revision) = revision_text(argument) => match agent.plan_state() {
            _ if revision.is_empty() => {
                state.notice("Usage: /plan revise TEXT");
                PlanAction::None
            }
            Some(crate::session::PlanState::Ready { .. }) => PlanAction::Revise(revision.into()),
            Some(crate::session::PlanState::Draft { .. }) => {
                state.notice(
                    "The plan is still a draft. Finish it, or run /plan resume to continue it.",
                );
                PlanAction::None
            }
            None => {
                state.notice("No active plan.");
                PlanAction::None
            }
        },
        objective => {
            let replaced = agent.active_plan().is_some();
            match agent.start_plan(objective.to_string().into()) {
                Ok(warning) => {
                    state.active_plan = None;
                    state.plan_draft = true;
                    if let Some(warning) = warning {
                        state.notice(warning);
                    }
                    if replaced {
                        state.notice(
                            "Started a new plan in place of the ready one. /undo restores it.",
                        );
                    }
                    PlanAction::Start
                }
                Err(error) => {
                    state.notice(format!("Could not start the plan: {error}"));
                    PlanAction::None
                }
            }
        }
    }
}

fn notice_plan_status(agent: &Agent, state: &mut ViewState) {
    match agent.plan_state() {
        Some(crate::session::PlanState::Draft { objective, .. }) => state.notice(format!(
            "Draft plan for:\n{objective}\n\n/plan resume continues it. /plan cancel clears it."
        )),
        Some(crate::session::PlanState::Ready { plan, .. }) => state.notice(format!(
            "Active plan:\n\n{plan}\n\nEnter a prompt, or run /plan revise TEXT or /plan implement. /plan cancel clears it."
        )),
        None => state.notice("No active plan. Start one with /plan TEXT."),
    }
}

pub(in crate::tui) fn plan_handoff_picker() -> Picker {
    Picker {
        title: "Plan ready".into(),
        hint: "Enter select · Esc return to editor".into(),
        selected: 0,
        items: vec![
            PickerItem {
                label: "Implement".into(),
                description: "Start implementing this plan now".into(),
                action: PickerAction::ImplementPlan,
            },
            PickerItem {
                label: "Return to editor".into(),
                description: "Write a change request or implementation prompt".into(),
                action: PickerAction::ReturnFromPlan,
            },
        ],
        editing: None,
        parent: Some(PickerAction::ReturnFromPlan),
    }
}

pub(in crate::tui) fn goal(agent: &mut Agent, argument: &str, state: &mut ViewState) -> GoalAction {
    match argument {
        "" => {
            notice_goal_status(agent, state, false);
            GoalAction::None
        }
        "cancel" => {
            match agent.cancel_goal() {
                Ok(true) => {
                    state.active_goal = None;
                    state.notice("Goal canceled.");
                }
                Ok(false) => state.notice("No active goal."),
                Err(error) => state.notice(format!("Could not cancel the goal: {error}")),
            }
            GoalAction::None
        }
        "resume" => {
            if agent.active_goal().is_none() {
                state.notice("No paused goal to resume.");
                return GoalAction::None;
            }
            GoalAction::Resume
        }
        goal => match agent.start_goal(goal.to_string().into()) {
            Ok(warning) => {
                state.active_goal = agent.active_goal().map(str::to_string);
                if let Some(warning) = warning {
                    state.notice(warning);
                }
                GoalAction::Start
            }
            Err(error) => {
                state.notice(format!("Could not start the goal: {error}"));
                GoalAction::None
            }
        },
    }
}

pub(in crate::tui) fn goal_while_busy(argument: &str, state: &mut ViewState) {
    match argument {
        "" => {
            if let Some(goal) = state.active_goal.as_deref() {
                if state.goal_running {
                    state.notice(format!("Working on goal:\n{goal}"));
                } else {
                    state.notice(format!(
                        "Paused goal:\n{goal}\n\nWait for the current turn to settle, then run /goal resume."
                    ));
                }
            } else {
                state.notice("No active goal.");
            }
        }
        "cancel" => state.notice("Wait for the current turn to settle, then run /goal cancel."),
        "resume" if state.goal_running => state.notice("A goal is already running."),
        "resume" => state.notice("Wait for the current turn to settle, then run /goal resume."),
        _ => state.notice("Wait for the current turn to settle before replacing the goal."),
    }
}

pub(in crate::tui) fn plan_while_busy(argument: &str, state: &mut ViewState) {
    match argument {
        "" => {
            if state.plan_draft {
                state.notice("A planning workflow is currently running.");
            } else if let Some(plan) = state.active_plan.as_deref() {
                state.notice(format!("Active plan:\n\n{plan}"));
            } else {
                state.notice("No active plan.");
            }
        }
        "implement" => {
            state.notice("Wait for the current turn to settle, then run /plan implement.")
        }
        argument if revision_text(argument).is_some() => {
            state.notice("Wait for the current turn to settle, then run /plan revise TEXT.");
        }
        _ => state.notice("Wait for the current turn to settle before changing the plan."),
    }
}

fn notice_goal_status(agent: &Agent, state: &mut ViewState, running: bool) {
    match agent.active_goal() {
        Some(goal) if running || state.turn_started.is_some() => {
            state.notice(format!("Working on goal:\n{goal}"));
        }
        Some(goal) => state.notice(format!(
            "Paused goal:\n{goal}\n\n/goal resume continues it. /goal cancel clears it."
        )),
        None => state.notice("No active goal. Start one with /goal TEXT."),
    }
}
