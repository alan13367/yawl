//! Slash commands, settings mutation, queue actions, skills, and session selection.
//!
//! This facade owns slash-command dispatch and session/queue actions. Private
//! children handle settings parsing/application and transcript copying. TUI
//! callers keep the entry points here.

mod copy;
mod settings;

pub(super) use copy::{copy_all_from_transcript, copy_all_messages, copy_last_reply};
#[cfg(test)]
pub(super) use copy::{format_copy_all, last_assistant_reply};
pub(super) use settings::{
    apply_config_change, notice_config_effect, reasoning, refresh_model_selection, settings,
    show_settings, show_usage,
};
use settings::{apply_reasoning_effort, interface_location, open_settings_location};

use crate::agent::Agent;
use crate::config::ConfigChange;
use crate::provider::TurnInput;

use super::picker::{
    Picker, PickerAction, PickerItem, SettingsCategory, SettingsItem, SettingsLocation,
    color_picker, open_model_picker, open_reasoning_picker, select_picker_item,
    selection_color_picker, settings_category_picker, settings_picker, status_bar_editor_picker,
    web_search_provider_picker,
};
use super::state::ViewState;

pub(super) const HELP: &str = "\
Commands

| Command | Description |
| --- | --- |
| `/new`, `/clear` | Start a fresh session in the current directory |
| `/resume [ID]` | Open session picker or resume directly |
| `/undo` | Restore files and drop the last turn |
| `/diff` | Show files changed this session |
| `/init` | Create or update project agent guidance |
| `/quit` | Leave Yawl |
| --- | --- |
| `/model [MODEL]` | Open model picker or switch directly |
| `/reasoning [LVL]` | Show supported reasoning levels or set one |
| `/settings [KEY]` | Open settings picker or change directly |
| `/connect` | Configure a model provider interactively |
| --- | --- |
| `/usage` | Show token and prompt-cache usage |
| `/compact` | Summarize older messages now |
| `/copy` | Copy the last assistant reply |
| `/copy-all` | Copy conversation without reasoning |
| `/tools` | List builtin and discovered tools |
| `/skills` | List discovered skills (run `/skill:NAME`) |
| --- | --- |
| `/goal [TEXT]` | Start, resume, cancel, or show goal |
| `/plan [TEXT]` | Start, resume, cancel, or show plan workflow |
| `/unqueue [N]` | Cancel or manage queued messages |
| `/subagents` | Open the subagent dashboard |
| `/git` | Open the git dashboard (offers repo setup outside a repo) |
| `/ps` | Open the background process dashboard |
| --- | --- |
| `/hotkeys` | Show keyboard shortcuts grid |
| `/help` | Show this command reference |

Input

- `Enter` submits when idle, or steers while a response runs
- `Tab` completes in menus, or queues a message while busy
- Run `/hotkeys` for all editing, transcript, and dashboard shortcuts
";

pub(super) const HOTKEYS: &str = "\
Keyboard shortcuts

| Area | Key | Action |
| --- | --- | --- |
| **Input** | `Enter` | Submit while idle; steer running turn |
| | `Ctrl+G`, `Ctrl+Enter` | Steer at next safe boundary |
| | `Tab` | Queue while busy; complete in menus |
| | `Shift/Alt+Enter`, `Ctrl+J` | Insert a newline |
| | `Escape`, `Ctrl+C` | Abort active turn; clear editor |
| | `Ctrl+L` | Repaint the screen |
| --- | --- | --- |
| **Editing** | `Left` / `Right` | Move cursor |
| | `Home` / `End`, `Ctrl+A/E` | Jump to line start / end |
| | `Backspace`, `Delete` | Delete character |
| | `Ctrl+U`, `Ctrl+K`, `Ctrl+W` | Delete to start, end, or word |
| | `Up` / `Down` | Move wrapped line, then history |
| | `Ctrl+V` | Paste image as `[Image #N]` |
| --- | --- | --- |
| **Menus** | `Up` / `Down` | Change selection (wraps) |
| | `Tab` | Complete selection |
| | `Enter` | Run command, or insert `@` tag |
| --- | --- | --- |
| **Transcript** | `Tab` | Focus transcript when idle |
| | `Up`/`Down` or `k`/`j` | Move between blocks |
| | `Left`/`Right` or `h`/`l` | Fold / unfold selected block |
| | `Enter` | Open full-screen viewer |
| | `y` | Copy selected block |
| | `Escape`, `Ctrl+C` | Return to editor |
| | `Ctrl+O` | Expand / collapse tool output |
| | `Ctrl+F` | Search: Enter next, Up prev, Esc closes |
| | `PgUp`/`PgDn`, Wheel | Scroll; drag scroll bar to jump |
| | Drag (left button) | Select text; release copies to clipboard |
| --- | --- | --- |
| **Viewer** | `Up`/`Down`, `PgUp`/`PgDn` | Scroll viewer |
| | `y` | Copy block contents |
| | `Escape`, `Enter` | Close viewer |
| --- | --- | --- |
| **Questions** | `Up`/`Down` or `k`/`j` | Change selection |
| | `1`–`4` | Choose option directly |
| | `Enter` | Confirm (Other… opens multiline editor) |
| | `Escape` | Cancel question and running turn |
| --- | --- | --- |
| **Queue** | `Up` / `Down` | Select queued message |
| | `K` / `J` | Reorder queued messages |
| | `e` | Edit selected message |
| | `d`, `Delete` | Delete selected message |
| | `Enter` | Stop turn and send now |
| --- | --- | --- |
| **/ps** | `Up`/`Down` or `j`/`k` | Select row; Enter views logs |
| | `x`, `Ctrl+C` | Stop command immediately |
| | `r` / `d`, `Delete` | Restart / remove settled history |
| | `Escape` | Close dashboard |
| --- | --- | --- |
| **/subagents** | `Up`/`Down` or `j`/`k` | Select run; Enter opens takeover |
| | `x` | Ask to cancel selected run |
| | `Escape` | Close dashboard |
| --- | --- | --- |
| **/git** | `Up`/`Down` or `j`/`k` | Select file; Enter opens diff |
| | `Space` | Stage / unstage selected file |
| | `+` / `-` | Stage / unstage selected file |
| | `↩` (undo) | Discard selected file (always confirms) |
| | `a` / `u` | Stage all / unstage all |
| | `e`, `Tab` | Edit commit message; Enter commits |
| | `C`, `m` | Commit now; toggle amend |
| | `v` | Commit menu: push, sync, amend |
| | `Tab` | Cycle files, history, and message |
| | `Enter` on history | View that commit's diff |
| | `d` | Discard selected file (always confirms) |
| | `p` / `f` / `F` | Push / fetch / pull |
| | `b` / `l` / `s` / `S` | Branches / log / stash / pop |
| | `r` | Refresh status |
| | `Escape` | Close diff, then dashboard |
";

pub(super) fn is_new_session_command(name: &str) -> bool {
    matches!(name, "new" | "clear")
}

pub(super) fn init(argument: &str) -> Result<TurnInput, &'static str> {
    if argument.is_empty() {
        Ok("/init".to_string().into())
    } else {
        Err("Usage: /init")
    }
}

pub(super) enum GoalAction {
    None,
    Start,
    Resume,
}

pub(super) enum PlanAction {
    None,
    Start,
    Resume,
}

pub(super) fn plan(agent: &mut Agent, argument: &str, state: &mut ViewState) -> PlanAction {
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
                    "The plan is already complete. Enter a prompt to revise or implement it.",
                );
                PlanAction::None
            }
            None => {
                state.notice("No interrupted plan to resume.");
                PlanAction::None
            }
        },
        objective => match agent.start_plan(objective.to_string().into()) {
            Ok(warning) => {
                state.active_plan = None;
                state.plan_draft = true;
                if let Some(warning) = warning {
                    state.notice(warning);
                }
                PlanAction::Start
            }
            Err(error) => {
                state.notice(format!("Could not start the plan: {error}"));
                PlanAction::None
            }
        },
    }
}

fn notice_plan_status(agent: &Agent, state: &mut ViewState) {
    match agent.plan_state() {
        Some(crate::session::PlanState::Draft { objective, .. }) => state.notice(format!(
            "Draft plan for:\n{objective}\n\n/plan resume continues it. /plan cancel clears it."
        )),
        Some(crate::session::PlanState::Ready { plan }) => state.notice(format!(
            "Active plan:\n\n{plan}\n\nEnter a prompt to revise or implement it. /plan cancel clears it."
        )),
        None => state.notice("No active plan. Start one with /plan TEXT."),
    }
}

pub(super) fn plan_handoff_picker() -> Picker {
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

pub(super) fn goal(agent: &mut Agent, argument: &str, state: &mut ViewState) -> GoalAction {
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

pub(super) fn goal_while_busy(argument: &str, state: &mut ViewState) {
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

pub(super) fn plan_while_busy(argument: &str, state: &mut ViewState) {
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

pub(super) fn show_skills(agent: &Agent, state: &mut ViewState) {
    let catalog = crate::skills::discover(agent.config());
    let mut text = String::from("Skill directories\n\n");
    for dir in &catalog.directories {
        text.push_str(&format!("- `{}`\n", dir.display()));
    }
    if !agent.config().project_skills_trusted()
        && crate::skills::has_project_sources(agent.config())
    {
        text.push_str("\nProject skill sources are not trusted for this invocation.\n");
    }
    if catalog.skills.is_empty() {
        text.push_str("\nNo skills found. Add one with `/settings skills add DIRECTORY`.");
    } else {
        text.push_str("\nAvailable skills\n\n");
        for skill in catalog.skills {
            let mode = if skill.disable_model_invocation {
                " (manual only)"
            } else {
                ""
            };
            text.push_str(&format!(
                "- `/skill:{}`{mode}: {}\n",
                skill.name, skill.description,
            ));
        }
    }
    if !catalog.warnings.is_empty() {
        text.push_str("\nRejected skills\n\n");
        for warning in catalog.warnings {
            text.push_str(&format!("- {warning}\n"));
        }
    }
    state.notice(text);
}

pub(super) fn queue_picker(state: &ViewState, selected: usize) -> Option<Picker> {
    if state.queued_inputs.is_empty() {
        return None;
    }
    let mut items = state
        .queued_inputs
        .iter()
        .enumerate()
        .map(|(index, input)| PickerItem {
            label: format!("Queued {}", index + 1),
            description: input.text.replace('\n', " "),
            action: PickerAction::SendQueued(index),
        })
        .collect::<Vec<_>>();
    items.push(PickerItem {
        label: "Clear all queued messages".into(),
        description: format!("Remove all {} pending", state.queued_inputs.len()),
        action: PickerAction::ClearQueued,
    });
    Some(Picker {
        title: "Queued messages".into(),
        hint: "↑/↓ select  K/J reorder  e edit  d delete  Enter send now".into(),
        selected: selected.min(items.len().saturating_sub(1)),
        items,
        editing: None,
        parent: None,
    })
}

pub(super) fn open_queue_picker(state: &mut ViewState) {
    state.picker = queue_picker(state, 0);
    if state.picker.is_none() {
        state.activity = "no queued messages".into();
    }
}

pub(super) fn remove_queued(state: &mut ViewState, index: usize) -> bool {
    if state.queued_inputs.remove(index).is_some() {
        state.activity = format!("removed queued message {}", index + 1);
        state.scroll_offset = 0;
        true
    } else {
        state.activity = format!("queued message {} does not exist", index + 1);
        false
    }
}

pub(super) fn clear_queued(state: &mut ViewState) {
    let count = state.queued_inputs.len();
    state.queued_inputs.clear();
    state.activity = match count {
        0 => "no queued messages".into(),
        1 => "removed 1 queued message".into(),
        _ => format!("removed {count} queued messages"),
    };
    state.scroll_offset = 0;
}

pub(super) fn move_queued(state: &mut ViewState, index: usize, direction: isize) -> usize {
    if state.queued_inputs.is_empty() || index >= state.queued_inputs.len() {
        return index;
    }
    let target = if direction < 0 {
        index.saturating_sub(direction.unsigned_abs())
    } else {
        index
            .saturating_add(direction as usize)
            .min(state.queued_inputs.len() - 1)
    };
    if target != index
        && let Some(input) = state.queued_inputs.remove(index)
    {
        state.queued_inputs.insert(target, input);
        state.activity = format!("moved queued message {} to {}", index + 1, target + 1);
    }
    target
}

pub(super) fn promote_queued(state: &mut ViewState, index: usize) -> bool {
    if index >= state.queued_inputs.len() {
        return false;
    }
    let _ = move_queued(state, index, -(index as isize));
    state.activity = "stopping the active turn to send queued message".into();
    true
}

pub(super) fn unqueue(argument: &str, state: &mut ViewState) {
    match argument {
        "" => open_queue_picker(state),
        "all" => clear_queued(state),
        number => match number
            .parse::<usize>()
            .ok()
            .and_then(|number| number.checked_sub(1))
        {
            Some(index) => {
                let _ = remove_queued(state, index);
            }
            None => state.activity = "usage: /unqueue [NUMBER|all]".into(),
        },
    }
}

pub(super) fn handle_queue_picker_action(
    state: &mut ViewState,
    action: PickerAction,
) -> Option<PickerAction> {
    match action {
        PickerAction::ApplyQueued { index, value } => {
            if let Some(input) = state.queued_inputs.get_mut(index) {
                input.set_text(value);
                state.activity = format!("updated queued message {}", index + 1);
            }
            state.picker = queue_picker(state, index);
            None
        }
        PickerAction::MoveQueued { index, direction } => {
            let selected = move_queued(state, index, direction);
            state.picker = queue_picker(state, selected);
            None
        }
        PickerAction::RemoveQueued(index) => {
            if remove_queued(state, index) {
                state.picker = queue_picker(state, index);
            }
            None
        }
        PickerAction::ClearQueued => {
            clear_queued(state);
            None
        }
        action => Some(action),
    }
}

pub(super) fn activate_picker_action(
    agent: &mut Agent,
    state: &mut ViewState,
    action: PickerAction,
) {
    let Some(action) = handle_queue_picker_action(state, action) else {
        return;
    };
    if let PickerAction::OpenConnect { from_settings } = action {
        super::connection::open(state, agent.config(), from_settings);
        return;
    }
    let Some(action) = super::connection::handle_action(state, action) else {
        return;
    };
    let Some(action) = super::status_bar::handle_editor_action(state, action) else {
        return;
    };
    match action {
        PickerAction::SwitchModel(model) => {
            agent.switch_model(model);
            refresh_model_selection(agent, state);
            if crate::model::is_codex(agent.config(), agent.model()) {
                open_reasoning_picker(agent, state, false);
            } else {
                state.notice(format!("Switched to {}.", agent.model()));
            }
        }
        PickerAction::SaveModel(model) => {
            if settings(agent, &format!("model {model}"), state) {
                if crate::model::is_codex(agent.config(), agent.model()) {
                    open_reasoning_picker(agent, state, true);
                } else {
                    open_settings_location(
                        agent,
                        state,
                        SettingsLocation {
                            category: SettingsCategory::Model,
                            item: SettingsItem::DefaultModel,
                        },
                    );
                }
            }
        }
        PickerAction::OpenModels { save } => open_model_picker(agent, state, save),
        PickerAction::OpenReasoning { save } => open_reasoning_picker(agent, state, save),
        PickerAction::SetReasoning { effort, save } => {
            if save {
                let value = effort.as_deref().unwrap_or("default");
                if settings(agent, &format!("reasoning_effort {value}"), state) {
                    open_settings_location(
                        agent,
                        state,
                        SettingsLocation {
                            category: SettingsCategory::Model,
                            item: SettingsItem::ReasoningEffort,
                        },
                    );
                }
            } else {
                apply_reasoning_effort(agent, effort, state);
            }
        }
        PickerAction::SetHideReasoning(enabled) => {
            if apply_config_change(agent, ConfigChange::HideReasoning(enabled), state) {
                open_settings_location(
                    agent,
                    state,
                    interface_location(SettingsItem::ReasoningDisplay),
                );
            }
        }
        PickerAction::OpenAccentColor => {
            state.picker = Some(color_picker(agent.config().accent_color));
        }
        PickerAction::OpenSelectionColor => {
            state.picker = Some(selection_color_picker(agent.config().selection_color));
        }
        PickerAction::OpenWebSearchProviders => {
            state.picker = Some(web_search_provider_picker(
                agent.config().web_search_provider,
            ));
        }
        PickerAction::SetSelectionColor(selection) => {
            if apply_config_change(agent, ConfigChange::SelectionColor(selection), state) {
                open_settings_location(
                    agent,
                    state,
                    interface_location(SettingsItem::SelectionColor),
                );
            }
        }
        PickerAction::CancelStatusBarEditor => {
            state.status_bar_draft = None;
            open_settings_location(agent, state, interface_location(SettingsItem::StatusBar));
        }
        PickerAction::SaveStatusBar => {
            let Some(layout) = state.status_bar_draft.take() else {
                open_settings_location(agent, state, interface_location(SettingsItem::StatusBar));
                state.refresh_completions(agent);
                return;
            };
            if apply_config_change(agent, ConfigChange::StatusBar(layout.clone()), state) {
                open_settings_location(agent, state, interface_location(SettingsItem::StatusBar));
            } else {
                state.status_bar_draft = Some(layout);
                state.picker = Some(status_bar_editor_picker(state, 0));
            }
        }
        PickerAction::SetScrollBar(enabled) => {
            if apply_config_change(agent, ConfigChange::ScrollBar(enabled), state) {
                open_settings_location(agent, state, interface_location(SettingsItem::ScrollBar));
            }
        }
        PickerAction::SetScrollBarAutoHide(enabled) => {
            if apply_config_change(agent, ConfigChange::ScrollBarAutoHide(enabled), state) {
                open_settings_location(
                    agent,
                    state,
                    interface_location(SettingsItem::ScrollBarAutoHide),
                );
            }
        }
        PickerAction::SetBell(enabled) => {
            if apply_config_change(agent, ConfigChange::Bell(enabled), state) {
                open_settings_location(agent, state, interface_location(SettingsItem::Bell));
            }
        }
        PickerAction::SetAccentColor(color) => {
            if apply_config_change(agent, ConfigChange::AccentColor(color), state) {
                open_settings_location(agent, state, interface_location(SettingsItem::AccentColor));
            }
        }
        PickerAction::ResumeSession(id) => load_session(agent, &id, state),
        PickerAction::DeleteSession(id) => delete_session(agent, &id, state),
        PickerAction::OpenResume { selected } => {
            open_resume_picker(agent, state);
            select_picker_item(state, selected);
        }
        PickerAction::OpenSettingsRoot { selected } => {
            state.picker = Some(settings_picker(agent));
            select_picker_item(state, selected);
        }
        PickerAction::OpenSettingsCategory { category, selected } => {
            state.picker = Some(settings_category_picker(agent, category, selected));
        }
        PickerAction::ApplyConnectionPlan(plan) => {
            let session_model = plan.session_model().map(str::to_string);
            match agent.change_global_config_batch(plan.changes_for_save()) {
                Ok(effects) => {
                    for effect in effects {
                        notice_config_effect(agent.config(), effect, state);
                    }
                    if let Some(model) = session_model {
                        agent.switch_model(model);
                    }
                    refresh_model_selection(agent, state);
                    state.notice(format!("{} connection saved.", plan.provider_label));
                }
                Err(error) => state.notice(format!("Could not save connection: {error}")),
            }
        }
        PickerAction::ApplySetting { argument, location } => {
            if settings(agent, &argument, state)
                && let Some(location) = location
            {
                open_settings_location(agent, state, location);
            }
        }
        PickerAction::SetAutoCompact(enabled) => {
            if apply_config_change(agent, ConfigChange::AutoCompact(enabled), state) {
                open_settings_location(
                    agent,
                    state,
                    SettingsLocation {
                        category: SettingsCategory::Context,
                        item: SettingsItem::AutoCompact,
                    },
                );
            }
        }
        PickerAction::SetWebBrowsing(enabled) => {
            if apply_config_change(agent, ConfigChange::WebBrowsing(enabled), state) {
                open_settings_location(
                    agent,
                    state,
                    SettingsLocation {
                        category: SettingsCategory::Web,
                        item: SettingsItem::WebBrowsingEnabled,
                    },
                );
            }
        }
        PickerAction::SetWebSearchProvider(provider) => {
            if apply_config_change(agent, ConfigChange::WebSearchProvider(provider), state) {
                open_settings_location(
                    agent,
                    state,
                    SettingsLocation {
                        category: SettingsCategory::Web,
                        item: SettingsItem::WebSearchProvider,
                    },
                );
            }
        }
        PickerAction::SetSubagents(enabled) => {
            if apply_config_change(agent, ConfigChange::Subagents(enabled), state) {
                open_settings_location(
                    agent,
                    state,
                    SettingsLocation {
                        category: SettingsCategory::Subagents,
                        item: SettingsItem::SubagentsEnabled,
                    },
                );
            }
        }
        PickerAction::Reload => {
            if apply_config_change(agent, ConfigChange::Reload, state) {
                open_settings_location(
                    agent,
                    state,
                    SettingsLocation {
                        category: SettingsCategory::Advanced,
                        item: SettingsItem::Reload,
                    },
                );
            }
        }
        PickerAction::ShowSettings => show_settings(agent, state),
        PickerAction::ImplementPlan => {
            state.pending_plan_implementation = true;
        }
        PickerAction::ReturnFromPlan => {}
        PickerAction::OpenStatusBarEditor { .. }
        | PickerAction::OpenStatusBarItem { .. }
        | PickerAction::OpenStatusBarFormats(_)
        | PickerAction::SetStatusBarFormat { .. }
        | PickerAction::SetStatusBarVisibility { .. }
        | PickerAction::EditStatusBarLabel { .. }
        | PickerAction::ApplyStatusBarLabel { .. }
        | PickerAction::ResetStatusBarLabel(_)
        | PickerAction::OpenStatusBarAdd
        | PickerAction::AddStatusBarItem(_)
        | PickerAction::MoveStatusBarItem { .. }
        | PickerAction::RemoveStatusBarItem(_)
        | PickerAction::OpenStatusBarStyles
        | PickerAction::SetStatusBarStyle(_)
        | PickerAction::OpenStatusBarSeparators
        | PickerAction::SetStatusBarSeparator(_)
        | PickerAction::EditStatusBarSeparator(_)
        | PickerAction::ApplyStatusBarSeparator(_)
        | PickerAction::ResetStatusBar => {
            unreachable!("status-bar editor action should be consumed before dispatch")
        }
        PickerAction::EditSetting { .. }
        | PickerAction::EditSecretSetting { .. }
        | PickerAction::EditModel { .. }
        | PickerAction::EditConnect { .. }
        | PickerAction::ApplyConnect { .. }
        | PickerAction::ConnectChooseProvider(_)
        | PickerAction::ConnectCredential(_)
        | PickerAction::ConnectChooseModel(_)
        | PickerAction::ConnectRetry
        | PickerAction::ConnectCancelJob
        | PickerAction::CloseConnect
        | PickerAction::ConnectBack(_)
        | PickerAction::ConnectActivation(_)
        | PickerAction::OpenConnect { .. } => {}
        PickerAction::SendQueued(index) => {
            let _ = promote_queued(state, index);
        }
        PickerAction::ApplyQueued { .. }
        | PickerAction::MoveQueued { .. }
        | PickerAction::RemoveQueued(_)
        | PickerAction::ClearQueued => {}
    }
    state.refresh_completions(agent);
}

pub(super) fn open_resume_picker(agent: &Agent, state: &mut ViewState) {
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
        parent: None,
    });
}

/// Per-side content cap for `/diff` cards, matching the `read_file` limit.
const DIFF_MAX_BYTES: usize = 1024 * 1024;

/// Renders `/diff`: one diff card per file the file tools touched this
/// session, comparing the oldest `/undo` pre-image with the current contents.
pub(super) fn show_diff(agent: &Agent, state: &mut ViewState) {
    let work_tree = crate::config::working_dir();
    show_diff_files(state, &work_tree, &agent.touched_files());
}

/// Renders diff cards for explicit touched-file records. Split from
/// `show_diff` so tests can drive real files without checkpoint plumbing.
pub(super) fn show_diff_files(
    state: &mut ViewState,
    work_tree: &std::path::Path,
    files: &[crate::checkpoint::TouchedFile],
) {
    let mut skipped: Vec<String> = Vec::new();
    let mut shown = 0usize;
    for file in files {
        let display = diff_display_path(work_tree, &file.path);
        let previous = match (file.existed, &file.previous) {
            (true, Some(bytes)) => Some(bytes.clone()),
            (true, None) => {
                skipped.push(format!("{display} (pre-image unreadable)"));
                continue;
            }
            (false, _) => None,
        };
        let new_bytes = match std::fs::read(&file.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if previous.is_none() {
                    // Created and deleted again within the session.
                    continue;
                }
                Vec::new()
            }
            Err(_) => {
                skipped.push(format!("{display} (unreadable)"));
                continue;
            }
        };
        let old_bytes = previous.unwrap_or_default();
        if old_bytes == new_bytes {
            continue;
        }
        let (Ok(old), Ok(new)) = (String::from_utf8(old_bytes), String::from_utf8(new_bytes))
        else {
            skipped.push(format!("{display} (not UTF-8)"));
            continue;
        };
        if old.len() > DIFF_MAX_BYTES || new.len() > DIFF_MAX_BYTES {
            skipped.push(format!("{display} (over 1 MiB)"));
            continue;
        }
        state
            .transcript
            .push_diff(display, super::tool_view::edit_diff(&old, &new));
        shown += 1;
    }
    if shown == 0 && skipped.is_empty() {
        state.notice(
            "No file changes recorded this session. Changes made through `shell` or exec tools are not tracked.",
        );
        return;
    }
    if !skipped.is_empty() {
        let mut text = String::from("Files not shown\n\n");
        for path in &skipped {
            text.push_str(&format!("- {path}\n"));
        }
        state.notice(text);
    }
}

fn diff_display_path(work_tree: &std::path::Path, path: &str) -> String {
    std::path::Path::new(path)
        .strip_prefix(work_tree)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| path.to_string())
}

pub(super) fn notice_undo(state: &mut ViewState, report: crate::agent::UndoReport) {
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

pub(super) fn resume(agent: &mut Agent, selector: &str, state: &mut ViewState) {
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

pub(super) fn load_session(agent: &mut Agent, id: &str, state: &mut ViewState) {
    match agent.load_session(id) {
        Ok(()) => {
            let queued_inputs = std::mem::take(&mut state.queued_inputs);
            let pending_actions = std::mem::take(&mut state.pending_actions);
            *state = ViewState::from_agent(agent);
            state.queued_inputs = queued_inputs;
            state.pending_actions = pending_actions;
            state.notice(format!("Resumed session {id}."));
        }
        Err(error) => state.notice(format!("Could not resume '{id}': {error}")),
    }
}

pub(super) fn delete_session(agent: &mut Agent, id: &str, state: &mut ViewState) {
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
