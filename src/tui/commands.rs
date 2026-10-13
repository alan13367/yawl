//! Slash commands, settings mutation, queue actions, skills, and session selection.
//!
//! This facade owns slash-command help and picker-action dispatch. Private
//! children handle settings, goal and plan commands, the message queue,
//! sessions, `/diff`, and transcript copying. TUI callers keep the entry
//! points here.

mod copy;
mod diff;
mod modes;
mod queue;
mod sessions;
mod settings;

pub(super) use copy::{copy_all_from_transcript, copy_all_messages, copy_last_reply};
#[cfg(test)]
pub(super) use copy::{format_copy_all, last_assistant_reply};
pub(super) use diff::show_diff;
#[cfg(test)]
pub(super) use diff::show_diff_files;
pub(super) use modes::{
    GoalAction, PlanAction, goal, goal_while_busy, init, plan, plan_handoff_picker,
    plan_while_busy, revision_text,
};
#[cfg(test)]
pub(super) use queue::open_queue_picker;
pub(super) use queue::{handle_queue_picker_action, promote_queued, unqueue};
pub(super) use sessions::{
    delete_session, is_new_session_command, load_session, new_session, notice_undo,
    open_resume_picker, resume,
};
pub(super) use settings::{
    apply_config_change, notice_config_effect, reasoning, refresh_model_selection, settings,
    show_settings, show_usage, sync_display_settings,
};
use settings::{apply_reasoning_effort, interface_location, open_settings_location};

use crate::agent::Agent;
use crate::config::ConfigChange;

use super::picker::{
    PickerAction, SettingsCategory, SettingsItem, SettingsLocation, color_picker,
    open_model_picker, open_reasoning_picker, open_subagent_model_picker, remove_model_confirm,
    select_picker_item, selection_color_picker, settings_category_picker, settings_picker,
    status_bar_editor_picker, web_search_provider_picker,
};
use super::state::ViewState;

pub(super) const HELP: &str = "\
Commands

| Command | Description |
| --- | --- |
| `/new`, `/clear` | Start a fresh session in the current directory |
| `/resume [ID]` | Open session picker or resume directly |
| `/undo` | Restore files and drop the last turn |
| `/continue` | Resume a turn that failed or was interrupted |
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
| `/tools` | List builtin tools and your custom tools |
| `/skills` | List discovered skills (run `/skill:NAME`) |
| --- | --- |
| `/goal [TEXT]` | Start, resume, cancel, or show goal |
| `/plan [TEXT]` | Start, revise, implement, resume, cancel, or show plan |
| `/unqueue [N]` | Cancel or manage queued messages |
| `/subagents` | Open the subagent dashboard |
| `/git` | Open the git dashboard (offers repo setup outside a repo) |
| `/ps` | Open the background process dashboard |
| `/remote [off]` | Control this session from a device in your tailnet |
| --- | --- |
| `/hotkeys` | Show keyboard shortcuts grid |
| `/help` | Show this command reference |

Input

- `Enter` submits when idle, or steers while a response runs
- `Enter` on an empty prompt continues a turn that just failed
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
| | `Ctrl+O` | Expand / collapse all tool output |
| | Click | Expand / collapse that tool card only |
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

/// Lists builtin tools, then the custom tools saved for this user, so the
/// personalization the model added is visible and easy to find on disk.
pub(super) fn show_tools(agent: &mut Agent, state: &mut ViewState) {
    let registry = agent.scan_tools();
    let custom = registry.custom_tool_names();
    let (saved, builtin): (Vec<_>, Vec<_>) = registry
        .describe_all()
        .into_iter()
        .partition(|(name, _, _)| custom.contains(name));
    let mut text = String::from("Builtin tools\n\n");
    for (tool, description, origin) in builtin {
        text.push_str(&format!("- `{tool}` ({origin}): {description}\n"));
    }
    text.push_str("\nCustom tools\n\n");
    if saved.is_empty() {
        text.push_str(
            "None yet. Ask Yawl to turn a command or workflow you repeat into a tool; it saves \
             tools as JSON in `~/.yawl/tools/` or `./.yawl/tools/` and keeps them across \
             sessions.\n",
        );
    }
    for (tool, description, origin) in saved {
        text.push_str(&format!("- `{tool}` ({origin}): {description}\n"));
    }
    if !agent.config().project_skills_trusted() && crate::tools::has_project_tools(agent.config()) {
        text.push_str("\nProject tools in `./.yawl/tools/` are not trusted for this invocation.\n");
    }
    if !registry.warnings.is_empty() {
        text.push_str("\nSkipped\n\n");
        for warning in &registry.warnings {
            text.push_str(&format!("- {warning}\n"));
        }
    }
    state.notice(text);
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

/// Switches the session model and follows up with the reasoning picker for
/// models with supported levels. The session log records the switch so a
/// later resume keeps using it.
pub(super) fn switch_model(agent: &mut Agent, model: String, state: &mut ViewState) {
    match agent.switch_model(model) {
        Ok(()) => {
            refresh_model_selection(agent, state);
            if !crate::model::reasoning_efforts(agent.config(), agent.model()).is_empty() {
                open_reasoning_picker(agent, state, false);
            } else {
                state.notice(format!("Switched to {}.", agent.model()));
            }
        }
        Err(error) => state.notice(format!("Could not switch model: {error}")),
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
        PickerAction::SwitchModel(model) => switch_model(agent, model, state),
        PickerAction::SaveModel(model) => {
            if settings(agent, &format!("model {model}"), state) {
                if !crate::model::reasoning_efforts(agent.config(), agent.model()).is_empty() {
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
        PickerAction::ReturnToModels { save, selected } => {
            open_model_picker(agent, state, save);
            select_picker_item(state, selected);
        }
        PickerAction::ConfirmRemoveModel {
            model,
            save,
            selected,
        } => match remove_model_confirm(agent.config(), &model, save, selected) {
            Ok(picker) => state.picker = Some(picker),
            Err(message) => {
                state.notice(message);
                open_model_picker(agent, state, save);
                select_picker_item(state, selected);
            }
        },
        PickerAction::RemoveModel {
            model,
            save,
            selected,
        } => {
            remove_model(agent, &model, state);
            open_model_picker(agent, state, save);
            select_picker_item(state, selected);
        }
        PickerAction::OpenSubagentModels => open_subagent_model_picker(agent.config(), state),
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
            state.begin_color_preview(agent.config().selection_color);
            state.picker = Some(color_picker(agent.config().accent_color));
        }
        PickerAction::OpenSelectionColor => {
            state.begin_color_preview(agent.config().selection_color);
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
        PickerAction::SetTheme(theme) => {
            if apply_config_change(agent, ConfigChange::Theme(theme), state) {
                open_settings_location(agent, state, interface_location(SettingsItem::Theme));
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
            let use_this_session = session_model.is_some();
            match agent.change_global_config_batch(plan.changes_for_save()) {
                Ok(effects) => {
                    for effect in effects {
                        notice_config_effect(agent.config(), effect, state);
                    }
                    let switch_error = session_model
                        .map(|model| agent.switch_model(model))
                        .transpose()
                        .err();
                    refresh_model_selection(agent, state);
                    if let Some(error) = switch_error {
                        state.notice(format!(
                            "{} connection saved, but could not switch model: {error}.",
                            plan.provider_label
                        ));
                    } else {
                        state.notice(format!("{} connection saved.", plan.provider_label));
                        if use_this_session
                            && !crate::model::reasoning_efforts(agent.config(), agent.model())
                                .is_empty()
                        {
                            open_reasoning_picker(agent, state, false);
                        }
                    }
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
        | PickerAction::ConnectToggleReasoning(_)
        | PickerAction::ConnectConfirmReasoning
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

fn remove_model(agent: &mut Agent, model: &str, state: &mut ViewState) {
    let Some((provider, id)) = model.split_once(':') else {
        state.notice(format!("`{model}` is not a configured provider model."));
        return;
    };
    let change = ConfigChange::RemoveProviderModel {
        name: provider.to_string(),
        model: id.to_string(),
    };
    if apply_config_change(agent, change, state)
        && agent
            .config()
            .providers
            .get(provider)
            .is_none_or(|provider| provider.models.iter().all(|entry| entry.id != id))
    {
        state.notice(format!("Removed `{model}` from the model list."));
    }
}
