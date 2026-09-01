//! Slash commands, settings mutation, queue actions, skills, and session selection.

use crate::agent::Agent;
use crate::config::{
    Config, ConfigChange, ConfigChangeEffect, MAX_SUBAGENT_REQUEST_BUDGET,
    MAX_SUBAGENT_TIMEOUT_SECS, MAX_WEB_FETCH_MAX_CHARS, SkillDirectoryAction, UiColor,
    parse_bounded, parse_builtin_api_key, parse_on_off, parse_threshold,
};
use crate::error::Error;
use crate::provider::{Message, Role};

use super::picker::{
    Picker, PickerAction, PickerItem, SettingsCategory, SettingsItem, SettingsLocation,
    color_picker, open_model_picker, open_reasoning_picker, select_picker_item,
    selection_color_picker, settings_category_picker, settings_item_index, settings_picker,
    status_bar_editor_picker, web_search_provider_picker,
};
use super::state::ViewState;

pub(super) const HELP: &str = "\
Commands
  /model [MODEL]       open the model picker or switch directly
  /connect             configure a model provider interactively
  /settings [KEY ...]  open the settings picker or change directly
  /new                 start a new session without changing directories
  /clear               alias for /new
  /compact             summarize older messages now
  /usage               show token and prompt-cache usage
  /undo                restore files and drop the last turn
  /copy                copy the last assistant reply
  /copy-all            copy the conversation without reasoning
  /tools               list builtin and discovered tools
  /skills              list discovered skills and search directories
  /subagents           open the subagent dashboard
  /ps                  open the background process dashboard
  /skill:NAME [ARGS]   run a discovered skill
  /resume [ID|NUMBER]  open the session picker or resume directly
  /unqueue [N|all]     cancel queued messages
  /goal [TEXT]         start, resume, cancel, or show the persistent goal
  /help                show this help
  /quit                leave Yawl

Input
  Enter submits while idle and steers during an active response. Tab queues a
    non-empty message while a response runs. Shift+Enter, Alt+Enter, or Ctrl+J
    inserts a newline.
    Type / for commands; Up/Down select, Tab completes, and Enter runs the selected
    command or an exact name such as /copy.
  Model, settings, and provider setup remain available during an active response.
  Queued messages appear below the response. Ctrl+G steers the running turn at the
    next safe boundary; Ctrl+Enter also works in terminals that report modified
    Enter. /unqueue opens an editor: K/J reorder, e edits, d deletes, and Enter
    stops the turn to send.
  Outside the menu, Up and Down browse input history. Ctrl+U, Ctrl+K, and Ctrl+W edit.
  Ctrl+V pastes a clipboard image as [Image #N] when the model accepts images.
  Otherwise, Tab focuses transcript blocks when no completion is open. Up/Down
    move, Left/Right fold, Enter opens,
    y copies, and Esc returns to the editor. Ctrl+F searches the transcript.
  Ctrl+O expands or collapses tool output. Esc or Ctrl+C aborts the active turn.
   Mouse wheel and PageUp/PageDown scroll. Click or drag the right-edge
   scroll bar to move through the transcript. Drag selects text; release copies it.
";

pub(super) fn is_new_session_command(name: &str) -> bool {
    matches!(name, "new" | "clear")
}

pub(super) enum GoalAction {
    None,
    Start,
    Resume,
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
            state.model = agent.model().to_string();
            state.context_window = agent.context_window();
            state.context_tokens = 0;
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
                agent.set_reasoning_effort(effort.clone());
                state.reasoning_effort = effort;
                let label = state
                    .reasoning_effort
                    .as_deref()
                    .unwrap_or("provider default");
                state.notice(format!("Using {} with {label} reasoning.", agent.model()));
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
                    state.model = agent.model().to_string();
                    state.context_window = agent.context_window();
                    state.context_tokens = 0;
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

fn interface_location(item: SettingsItem) -> SettingsLocation {
    SettingsLocation {
        category: SettingsCategory::Interface,
        item,
    }
}

fn open_settings_location(agent: &Agent, state: &mut ViewState, location: SettingsLocation) {
    state.picker = Some(settings_category_picker(
        agent,
        location.category,
        settings_item_index(location.category, location.item),
    ));
}

pub(super) fn settings(agent: &mut Agent, argument: &str, state: &mut ViewState) -> bool {
    if argument.is_empty() {
        show_settings(agent, state);
        return false;
    }

    let mut parts = argument.split_whitespace();
    let key = parts.next().unwrap_or_default();
    let change = match key {
        "reload" => {
            if parts.next().is_some() {
                Err(Error::Config("usage: /settings reload".into()))
            } else {
                Ok(ConfigChange::Reload)
            }
        }
        "model" => one_value(&mut parts, "usage: /settings model MODEL")
            .map(|model| ConfigChange::Model(model.to_string())),
        "max_tokens" => {
            one_value(&mut parts, "usage: /settings max_tokens NUMBER").and_then(|value| {
                value
                    .parse::<u32>()
                    .map(ConfigChange::MaxTokens)
                    .map_err(|_| Error::Config("max_tokens must be a positive integer".into()))
            })
        }
        "reasoning_effort" => one_value(
            &mut parts,
            "usage: /settings reasoning_effort default|minimal|low|medium|high|xhigh|max",
        )
        .and_then(reasoning_effort_change),
        "hide_reasoning" => one_value(&mut parts, "usage: /settings hide_reasoning on|off")
            .and_then(parse_on_off)
            .map(ConfigChange::HideReasoning),
        "accent_color" | "status_bar_color" | "text_box_color" => {
            one_value(&mut parts, "usage: /settings accent_color NAME|#RRGGBB")
                .and_then(|value| UiColor::parse(value).map_err(Error::Config))
                .map(ConfigChange::AccentColor)
        }
        "selection_color" => one_value(
            &mut parts,
            "usage: /settings selection_color accent|NAME|#RRGGBB",
        )
        .and_then(|value| UiColor::parse_selection(value).map_err(Error::Config))
        .map(ConfigChange::SelectionColor),
        "scroll_bar" => one_value(&mut parts, "usage: /settings scroll_bar on|off")
            .and_then(parse_on_off)
            .map(ConfigChange::ScrollBar),
        "scroll_bar_auto_hide" => {
            one_value(&mut parts, "usage: /settings scroll_bar_auto_hide on|off")
                .and_then(parse_on_off)
                .map(ConfigChange::ScrollBarAutoHide)
        }
        "auto_compact" => one_value(&mut parts, "usage: /settings auto_compact on|off")
            .and_then(parse_on_off)
            .map(ConfigChange::AutoCompact),
        "compact_threshold" => one_value(
            &mut parts,
            "usage: /settings compact_threshold FRACTION|PERCENT%",
        )
        .and_then(parse_threshold)
        .map(ConfigChange::CompactThreshold),
        "web_browsing" => one_value(&mut parts, "usage: /settings web_browsing on|off")
            .and_then(parse_on_off)
            .map(ConfigChange::WebBrowsing),
        "web_search_provider" => one_value(
            &mut parts,
            "usage: /settings web_search_provider duckduckgo|brave|firecrawl",
        )
        .and_then(|value| value.parse().map_err(Error::Config))
        .map(ConfigChange::WebSearchProvider),
        "web_fetch_max_chars" => {
            one_value(&mut parts, "usage: /settings web_fetch_max_chars NUMBER")
                .and_then(|value| {
                    parse_bounded(value, 1, MAX_WEB_FETCH_MAX_CHARS, "web_fetch_max_chars")
                })
                .map(ConfigChange::WebFetchMaxChars)
        }
        "brave_api_key" => one_value(&mut parts, "usage: /settings brave_api_key KEY|-")
            .and_then(parse_builtin_api_key)
            .map(ConfigChange::BraveApiKey),
        "firecrawl_api_key" => one_value(&mut parts, "usage: /settings firecrawl_api_key KEY|-")
            .and_then(parse_builtin_api_key)
            .map(ConfigChange::FirecrawlApiKey),
        "subagents" => one_value(&mut parts, "usage: /settings subagents on|off")
            .and_then(parse_on_off)
            .map(ConfigChange::Subagents),
        "max_subagents" => one_value(&mut parts, "usage: /settings max_subagents NUMBER")
            .and_then(|value| parse_bounded(value, 1, 16, "max_subagents"))
            .map(ConfigChange::MaxSubagents),
        "subagent_model" => one_value(&mut parts, "usage: /settings subagent_model inherit|MODEL")
            .map(|value| ConfigChange::SubagentModel(value.to_string())),
        "subagent_request_budget" => one_value(
            &mut parts,
            "usage: /settings subagent_request_budget NUMBER|0-for-unlimited",
        )
        .and_then(|value| {
            parse_bounded(
                value,
                0,
                MAX_SUBAGENT_REQUEST_BUDGET,
                "subagent_request_budget",
            )
        })
        .map(ConfigChange::SubagentRequestBudget),
        "subagent_timeout_secs" => one_value(
            &mut parts,
            "usage: /settings subagent_timeout_secs SECONDS|0-for-unlimited",
        )
        .and_then(|value| {
            parse_bounded(value, 0, MAX_SUBAGENT_TIMEOUT_SECS, "subagent_timeout_secs")
        })
        .map(ConfigChange::SubagentTimeoutSecs),
        "context_window" => one_value(&mut parts, "usage: /settings context_window TOKENS")
            .and_then(|value| {
                let window = value.parse::<u64>().map_err(|_| {
                    Error::Config("context_window must be a positive integer".into())
                })?;
                Ok(ConfigChange::ContextWindow {
                    model: agent.model().to_string(),
                    window,
                })
            }),
        "skills" => {
            let action = parts.next();
            let path = parts.next();
            if !matches!(action, Some("add" | "remove")) || path.is_none() || parts.next().is_some()
            {
                Err(Error::Config(
                    "usage: /settings skills add|remove DIRECTORY".into(),
                ))
            } else {
                ConfigChange::skill_directory(
                    agent.config(),
                    if action == Some("add") {
                        SkillDirectoryAction::Add
                    } else {
                        SkillDirectoryAction::Remove
                    },
                    path.unwrap_or_default(),
                )
            }
        }
        "anthropic_base_url" | "openai_base_url" => {
            one_value(&mut parts, "usage: /settings openai_base_url URL").map(|url| {
                if key == "anthropic_base_url" {
                    ConfigChange::AnthropicBaseUrl(url.to_string())
                } else {
                    ConfigChange::OpenAiBaseUrl(url.to_string())
                }
            })
        }
        "anthropic_api_key" | "openai_api_key" => {
            one_value(&mut parts, "usage: /settings anthropic_api_key KEY|-").and_then(|value| {
                if key == "anthropic_api_key" {
                    parse_builtin_api_key(value).map(ConfigChange::AnthropicApiKey)
                } else {
                    parse_builtin_api_key(value).map(ConfigChange::OpenAiApiKey)
                }
            })
        }
        "provider" => {
            let name = parts.next();
            let url = parts.next();
            let api_key = parts.next();
            if name.is_none() || url.is_none() || parts.next().is_some() {
                Err(Error::Config(
                    "usage: /settings provider NAME BASE_URL [API_KEY|-]".into(),
                ))
            } else {
                Ok(ConfigChange::Provider {
                    name: name.unwrap_or_default().to_string(),
                    base_url: url.unwrap_or_default().to_string(),
                    api_key: api_key.map(str::to_string),
                })
            }
        }
        _ => Err(Error::Config(format!(
            "unknown setting '{key}'; run /settings to list settings"
        ))),
    };

    match change {
        Ok(change) => apply_config_change(agent, change, state),
        Err(error) => {
            state.notice(format!("Could not change setting: {error}"));
            false
        }
    }
}

fn apply_config_change(agent: &mut Agent, change: ConfigChange, state: &mut ViewState) -> bool {
    match agent.change_global_config(change) {
        Ok(effect) => {
            state.model = agent.model().to_string();
            state.reasoning_effort = agent.config().reasoning_effort.clone();
            state.hide_reasoning = agent.config().hide_reasoning;
            state.accent_color = agent.config().accent_color;
            state.selection_color = agent.config().effective_selection_color();
            state.status_bar = agent.config().status_bar.clone();
            state.subagents_enabled = agent.config().subagents;
            state.sync_scroll_bar_config(agent.config());
            state.context_window = agent.context_window();
            notice_config_effect(agent.config(), effect, state);
            true
        }
        Err(error) => {
            state.notice(format!("Could not change setting: {error}"));
            false
        }
    }
}

pub(super) fn notice_config_effect(
    config: &Config,
    effect: ConfigChangeEffect,
    state: &mut ViewState,
) {
    match effect {
        ConfigChangeEffect::Applied => {}
        ConfigChangeEffect::Overridden => state.notice(format!(
            "Saved to `{}`, but project settings in `{}` remain effective.",
            config.global_config_path().display(),
            config.project_config_path().display()
        )),
        ConfigChangeEffect::SkillDirectoryNotConfigured(path) => state.notice(format!(
            "Skill directory `{}` is not configured.",
            path.display()
        )),
    }
}

fn reasoning_effort_change(value: &str) -> Result<ConfigChange, Error> {
    let effective = match value {
        "default" | "off" => None,
        "minimal" | "low" | "medium" | "high" | "xhigh" | "max" => Some(value.to_string()),
        _ => return Err(Error::Config("unsupported reasoning effort".into())),
    };
    Ok(ConfigChange::ReasoningEffort {
        stored: value.to_string(),
        effective,
    })
}

pub(super) fn show_settings(agent: &Agent, state: &mut ViewState) {
    let mut providers = agent.config().providers.iter().collect::<Vec<_>>();
    providers.sort_by_key(|(name, _)| name.as_str());
    let mut text = format!(
        "Settings\n\n- model: `{}`\n- max_tokens: `{}`\n- reasoning_effort: `{}`\n- hide_reasoning: `{}`\n- accent_color: `{}`\n- selection_color: `{}`\n- status_bar: `{} items, {} style`\n- scroll_bar: `{}`\n- scroll_bar_auto_hide: `{}`\n- auto_compact: `{}`\n- compact_threshold: `{:.0}%`\n- context_window for current model: `{}`\n- web_browsing: `{}`\n- web_search_provider: `{}`\n- web_fetch_max_chars: `{}`\n- brave_api_key: `{}`\n- firecrawl_api_key: `{}`\n- subagents: `{}`\n- max_subagents: `{}`\n- subagent_model: `{}`\n- subagent_request_budget: `{}`\n- subagent_timeout_secs: `{}`\n- anthropic_base_url: `{}`\n- openai_base_url: `{}`\n- anthropic_api_key: `{}`\n- openai_api_key: `{}`\n\nSkill directories\n\n",
        agent.model(),
        agent.config().max_tokens,
        agent
            .config()
            .reasoning_effort
            .as_deref()
            .unwrap_or("provider default"),
        agent.config().hide_reasoning,
        agent.config().accent_color.config_value(),
        crate::config::UiColor::selection_config_value(agent.config().selection_color),
        agent.config().status_bar.items.len(),
        agent.config().status_bar.style.as_str(),
        if agent.config().scroll_bar {
            "on"
        } else {
            "off"
        },
        if agent.config().scroll_bar_auto_hide {
            "on"
        } else {
            "off"
        },
        if agent.config().auto_compact {
            "on"
        } else {
            "off"
        },
        agent.config().compact_threshold * 100.0,
        agent.context_window(),
        if agent.config().web_browsing {
            "on"
        } else {
            "off"
        },
        agent.config().web_search_provider,
        agent.config().web_fetch_max_chars,
        configured_key_status("BRAVE_API_KEY", agent.config().brave_api_key.as_deref()),
        configured_key_status(
            "FIRECRAWL_API_KEY",
            agent.config().firecrawl_api_key.as_deref(),
        ),
        if agent.config().subagents {
            "on"
        } else {
            "off"
        },
        agent.config().max_subagents,
        agent.config().subagent_model,
        if agent.config().subagent_request_budget == 0 {
            "unlimited".to_string()
        } else {
            agent.config().subagent_request_budget.to_string()
        },
        if agent.config().subagent_timeout_secs == 0 {
            "unlimited".to_string()
        } else {
            format!("{}s", agent.config().subagent_timeout_secs)
        },
        agent.config().anthropic_base_url,
        agent.config().openai_base_url,
        if agent.config().anthropic_api_key.is_some() {
            "set"
        } else {
            "not set"
        },
        if agent.config().openai_api_key.is_some() {
            "set"
        } else {
            "not set"
        },
    );
    for dir in &agent.config().skill_dirs {
        text.push_str(&format!("- `{}`\n", dir.display()));
    }
    text.push_str("\nOpenAI-compatible providers\n\n");
    for (name, provider) in providers {
        let auth = if provider.api_key.is_some() {
            "configured key"
        } else {
            "no configured key"
        };
        text.push_str(&format!(
            "- `{name}`: `{}` ({auth}, {} listed models)\n",
            provider.base_url,
            provider.models.len()
        ));
    }
    text.push_str(&format!(
        "\nChanges are written to `{}`. Project settings in `./.yawl/config.json` override them.\n\nCommands\n\n- `/settings model MODEL`\n- `/settings max_tokens NUMBER`\n- `/settings reasoning_effort default|minimal|low|medium|high|xhigh|max`\n- `/settings hide_reasoning on|off`\n- `/settings accent_color NAME|#RRGGBB`\n- `/settings selection_color accent|NAME|#RRGGBB`\n- `/settings scroll_bar on|off`\n- `/settings scroll_bar_auto_hide on|off`\n- `/settings auto_compact on|off`\n- `/settings compact_threshold 85%`\n- `/settings context_window TOKENS`\n- `/settings web_browsing on|off`\n- `/settings web_search_provider duckduckgo|brave|firecrawl`\n- `/settings web_fetch_max_chars NUMBER`\n- `/settings brave_api_key KEY|-`\n- `/settings firecrawl_api_key KEY|-`\n- `/settings subagents on|off`\n- `/settings max_subagents NUMBER`\n- `/settings subagent_model inherit|MODEL`\n- `/settings subagent_request_budget NUMBER|0`\n- `/settings subagent_timeout_secs SECONDS|0`\n- `/settings skills add|remove DIRECTORY`\n- `/settings provider NAME BASE_URL [API_KEY|-]`\n- `/settings openai_base_url URL`\n- `/settings anthropic_base_url URL`\n- `/settings anthropic_api_key KEY|-`\n- `/settings openai_api_key KEY|-`\n- `/settings reload`\n\nUse an environment reference such as `$OMLX_API_KEY` instead of putting a secret directly in terminal history. Pass `-` as a key value to remove a saved key.",
        agent.config().global_config_path().display()
    ));
    state.notice(text);
}

pub(super) fn show_usage(state: &mut ViewState) {
    let main = state.usage;
    let children = state.subagent_manager.total_child_usage();
    let mut text = String::from("Token usage\n\nMain conversation\n");
    append_usage_summary(&mut text, main);
    if children.requests > 0 {
        text.push_str("\nSubagents\n");
        append_usage_summary(&mut text, children);
    }
    text.push_str(
        "\nProvider-reported totals are persisted for the main session. Cache writes are reported separately from fresh input.",
    );
    state.notice(text);
}

fn append_usage_summary(text: &mut String, usage: crate::provider::UsageSummary) {
    use std::fmt::Write as _;

    let tokens = usage.tokens;
    writeln!(text, "- Requests: {}", usage.requests).expect("writing to a String cannot fail");
    writeln!(
        text,
        "- Input: {} total, {} fresh, {} read from cache",
        super::render::format_token_count(tokens.input_tokens),
        super::render::format_token_count(tokens.fresh_input_tokens()),
        super::render::format_token_count(tokens.cached_input_tokens),
    )
    .expect("writing to a String cannot fail");
    writeln!(
        text,
        "- Cache writes: {}",
        super::render::format_token_count(tokens.cache_write_input_tokens),
    )
    .expect("writing to a String cannot fail");
    writeln!(
        text,
        "- Output: {}",
        super::render::format_token_count(tokens.output_tokens),
    )
    .expect("writing to a String cannot fail");
    if tokens.cache_details_reported {
        writeln!(text, "- Cache hit rate: {}%", usage.cache_hit_percent())
            .expect("writing to a String cannot fail");
    } else {
        text.push_str("- Prompt cache: not reported\n");
    }
    if usage.cache_resets > 0 {
        writeln!(
            text,
            "- Cache resets from compaction: {}",
            usage.cache_resets
        )
        .expect("writing to a String cannot fail");
    }
}

fn configured_key_status(environment: &str, stored: Option<&str>) -> &'static str {
    if std::env::var(environment).is_ok_and(|value| !value.trim().is_empty()) {
        "environment"
    } else if stored.is_some() {
        "config"
    } else {
        "not set"
    }
}

pub(super) fn one_value<'a>(
    parts: &mut impl Iterator<Item = &'a str>,
    usage: &str,
) -> Result<&'a str, Error> {
    let value = parts
        .next()
        .ok_or_else(|| Error::Config(usage.to_string()))?;
    if parts.next().is_some() {
        return Err(Error::Config(usage.to_string()));
    }
    Ok(value)
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

pub(super) fn last_assistant_reply<'a>(
    messages: &'a [Message],
    streaming: Option<&'a str>,
) -> Option<&'a str> {
    if let Some(text) = streaming.filter(|text| !text.is_empty()) {
        return Some(text);
    }
    messages.iter().rev().find_map(|message| {
        (message.role == Role::Assistant && !message.content.is_empty())
            .then_some(message.content.as_str())
    })
}

pub(super) fn format_copy_all(messages: &[Message], streaming: Option<&str>) -> String {
    let mut parts = Vec::new();
    for message in messages {
        match message.role {
            Role::User if message.is_hidden_control() => {}
            Role::User => parts.push(CopyPart::User(&message.content)),
            Role::Assistant => parts.push(CopyPart::Assistant(&message.content)),
            Role::Tool => {}
        }
    }
    if let Some(text) = streaming {
        parts.push(CopyPart::Assistant(text));
    }
    format_copy_parts(&parts)
}

pub(super) fn format_copy_all_from_transcript(
    transcript: &super::transcript::Transcript,
) -> String {
    let mut parts = Vec::new();
    for entry in transcript.entries() {
        match entry {
            super::transcript::Entry::User(content) | super::transcript::Entry::Steer(content) => {
                parts.push(CopyPart::User(content))
            }
            super::transcript::Entry::Assistant(content) => {
                parts.push(CopyPart::Assistant(content));
            }
            super::transcript::Entry::SubagentResult {
                id,
                name,
                status,
                content,
            } => {
                parts.push(CopyPart::OwnedUser(format!(
                    "[{id} {status}] {name}\n{content}"
                )));
            }
            _ => {}
        }
    }
    format_copy_parts(&parts)
}

enum CopyPart<'a> {
    User(&'a str),
    Assistant(&'a str),
    OwnedUser(String),
}

fn format_copy_parts(parts: &[CopyPart<'_>]) -> String {
    let mut blocks = Vec::new();
    let mut pending_user: Option<String> = None;
    let mut assistants: Vec<String> = Vec::new();
    let mut saw_empty_assistant = false;
    for part in parts {
        match part {
            CopyPart::User(content) => {
                flush_copy_turn(
                    &mut blocks,
                    &mut pending_user,
                    &mut assistants,
                    &mut saw_empty_assistant,
                );
                pending_user = Some((*content).to_string());
            }
            CopyPart::OwnedUser(content) => {
                flush_copy_turn(
                    &mut blocks,
                    &mut pending_user,
                    &mut assistants,
                    &mut saw_empty_assistant,
                );
                pending_user = Some(content.clone());
            }
            CopyPart::Assistant("") => {
                saw_empty_assistant = true;
            }
            CopyPart::Assistant(content) => assistants.push((*content).to_string()),
        }
    }
    flush_copy_turn(
        &mut blocks,
        &mut pending_user,
        &mut assistants,
        &mut saw_empty_assistant,
    );
    blocks.join("\n\n")
}

fn flush_copy_turn(
    blocks: &mut Vec<String>,
    pending_user: &mut Option<String>,
    assistants: &mut Vec<String>,
    saw_empty_assistant: &mut bool,
) {
    if assistants.is_empty() && *saw_empty_assistant {
        pending_user.take();
        *saw_empty_assistant = false;
        return;
    }
    if let Some(user) = pending_user.take() {
        blocks.push(format!("User:\n{user}"));
    }
    for text in assistants.drain(..) {
        blocks.push(format!("Assistant:\n{text}"));
    }
    *saw_empty_assistant = false;
}

pub(super) fn copy_to_clipboard(
    terminal: &mut super::terminal::Terminal,
    state: &mut ViewState,
    text: &str,
) -> Result<(), Error> {
    if text.is_empty() {
        state.notice("Nothing to copy.");
        return Ok(());
    }
    if terminal.copy_text(text)? {
        state.copy_toast_ticks = super::state::COPY_TOAST_TICKS;
    }
    Ok(())
}

pub(super) fn copy_last_reply(
    terminal: &mut super::terminal::Terminal,
    state: &mut ViewState,
    messages: &[Message],
) -> Result<(), Error> {
    let text = state
        .transcript
        .last_assistant_text()
        .map(str::to_string)
        .or_else(|| last_assistant_reply(messages, None).map(str::to_string))
        .unwrap_or_default();
    copy_to_clipboard(terminal, state, &text)
}

pub(super) fn copy_all_messages(
    terminal: &mut super::terminal::Terminal,
    state: &mut ViewState,
    messages: &[Message],
) -> Result<(), Error> {
    let streaming = state
        .transcript
        .has_streaming_assistant()
        .then(|| state.transcript.last_assistant_text().map(str::to_string))
        .flatten();
    let text = format_copy_all(messages, streaming.as_deref());
    copy_to_clipboard(terminal, state, &text)
}

pub(super) fn copy_all_from_transcript(
    terminal: &mut super::terminal::Terminal,
    state: &mut ViewState,
) -> Result<(), Error> {
    let text = format_copy_all_from_transcript(&state.transcript);
    copy_to_clipboard(terminal, state, &text)
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
