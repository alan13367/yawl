//! Slash commands, settings mutation, queue actions, skills, and session selection.

use crate::agent::Agent;
use crate::config::{Config, ConfigChange, ConfigChangeEffect, SkillDirectoryAction};
use crate::error::Error;

use super::picker::{
    Picker, PickerAction, PickerItem, SETTINGS_ACCENT_COLOR_INDEX, SETTINGS_AUTO_COMPACT_INDEX,
    SETTINGS_RELOAD_INDEX, color_picker, open_model_picker, open_reasoning_picker,
    open_settings_picker, select_picker_item,
};
use super::state::ViewState;

pub(super) const HELP: &str = "\
Commands
  /model [MODEL]       open the model picker or switch directly
  /settings [KEY ...]  open the settings picker or change directly
  /new                 start a new session without changing directories
  /clear               alias for /new
  /compact             summarize older messages now
  /tools               list builtin and discovered tools
  /skills              list discovered skills and search directories
  /skill:NAME [ARGS]   run a discovered skill
  /resume [ID|NUMBER]  open the session picker or resume directly
  /unqueue [N|all]     cancel queued messages
  /help                show this help
  /quit                leave Yawl

Input
  Enter submits. Shift+Enter or Alt+Enter inserts a newline.
  Type / for commands; Up/Down select, Tab completes, and Enter accepts a sole match.
  Model and settings pickers remain available during an active response.
  Messages submitted during a response appear below it as queued.
  Outside the menu, Up and Down browse input history. Ctrl+U, Ctrl+K, and Ctrl+W edit.
  Ctrl+O expands or collapses tool output. Esc or Ctrl+C aborts the active turn.
  Mouse wheel and PageUp/PageDown scroll. Drag selects text; release copies it.
";

pub(super) fn is_new_session_command(name: &str) -> bool {
    matches!(name, "new" | "clear")
}

pub(super) fn show_skills(agent: &Agent, state: &mut ViewState) {
    let skills = crate::skills::scan(agent.config());
    let mut text = String::from("Skill directories\n\n");
    for dir in &agent.config().skill_dirs {
        text.push_str(&format!("- `{}`\n", dir.display()));
    }
    if skills.is_empty() {
        text.push_str("\nNo skills found. Add one with `/settings skills add DIRECTORY`.");
    } else {
        text.push_str("\nAvailable skills\n\n");
        for skill in skills {
            text.push_str(&format!(
                "- `/skill:{}`: {}\n",
                skill.name, skill.description
            ));
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
            description: input.replace('\n', " "),
            action: PickerAction::RemoveQueued(index),
        })
        .collect::<Vec<_>>();
    items.push(PickerItem {
        label: "Clear all queued messages".into(),
        description: format!("Remove all {} pending", state.queued_inputs.len()),
        action: PickerAction::ClearQueued,
    });
    Some(Picker {
        title: "Queued messages".into(),
        hint: "↑/↓ move  Enter remove  Esc close".into(),
        selected: selected.min(items.len().saturating_sub(1)),
        items,
        editing: None,
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
                    open_settings_picker(agent, state);
                    select_picker_item(state, 0);
                }
            }
        }
        PickerAction::OpenModels { save } => open_model_picker(agent, state, save),
        PickerAction::OpenReasoning { save } => open_reasoning_picker(agent, state, save),
        PickerAction::SetReasoning { effort, save } => {
            if save {
                let value = effort.as_deref().unwrap_or("default");
                if settings(agent, &format!("reasoning_effort {value}"), state) {
                    open_settings_picker(agent, state);
                    select_picker_item(state, 2);
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
            if settings(
                agent,
                &format!("hide_reasoning {}", if enabled { "on" } else { "off" }),
                state,
            ) {
                open_settings_picker(agent, state);
                select_picker_item(state, 3);
            }
        }
        PickerAction::OpenAccentColor => {
            state.picker = Some(color_picker(agent.config().accent_color));
        }
        PickerAction::SetAccentColor(color) => {
            if settings(
                agent,
                &format!("accent_color {}", color.config_value()),
                state,
            ) {
                open_settings_picker(agent, state);
                select_picker_item(state, SETTINGS_ACCENT_COLOR_INDEX);
            }
        }
        PickerAction::ResumeSession(id) => load_session(agent, &id, state),
        PickerAction::ApplySetting { argument, selected } => {
            if settings(agent, &argument, state) {
                open_settings_picker(agent, state);
                select_picker_item(state, selected);
            }
        }
        PickerAction::SetAutoCompact(enabled) => {
            if settings(
                agent,
                &format!("auto_compact {}", if enabled { "on" } else { "off" }),
                state,
            ) {
                open_settings_picker(agent, state);
                select_picker_item(state, SETTINGS_AUTO_COMPACT_INDEX);
            }
        }
        PickerAction::Reload => {
            if settings(agent, "reload", state) {
                open_settings_picker(agent, state);
                select_picker_item(state, SETTINGS_RELOAD_INDEX);
            }
        }
        PickerAction::ShowSettings => show_settings(agent, state),
        PickerAction::EditSetting { .. } | PickerAction::EditModel { .. } => {}
        PickerAction::RemoveQueued(_) | PickerAction::ClearQueued => {}
    }
    state.refresh_completions(agent);
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
        "max_tokens" => one_value(&mut parts, "usage: /settings max_tokens NUMBER")
            .map(|value| ConfigChange::MaxTokens(value.to_string())),
        "reasoning_effort" => one_value(
            &mut parts,
            "usage: /settings reasoning_effort default|minimal|low|medium|high|xhigh|max",
        )
        .map(|value| ConfigChange::ReasoningEffort(value.to_string())),
        "hide_reasoning" => one_value(&mut parts, "usage: /settings hide_reasoning on|off")
            .map(|value| ConfigChange::HideReasoning(value.to_string())),
        "accent_color" | "status_bar_color" | "text_box_color" => {
            one_value(&mut parts, "usage: /settings accent_color NAME|#RRGGBB")
                .map(|value| ConfigChange::AccentColor(value.to_string()))
        }
        "auto_compact" => one_value(&mut parts, "usage: /settings auto_compact on|off")
            .map(|value| ConfigChange::AutoCompact(value.to_string())),
        "compact_threshold" => one_value(
            &mut parts,
            "usage: /settings compact_threshold FRACTION|PERCENT%",
        )
        .map(|value| ConfigChange::CompactThreshold(value.to_string())),
        "context_window" => {
            one_value(&mut parts, "usage: /settings context_window TOKENS").map(|value| {
                ConfigChange::ContextWindow {
                    model: agent.model().to_string(),
                    value: value.to_string(),
                }
            })
        }
        "skills" => {
            let action = parts.next();
            let path = parts.next();
            if !matches!(action, Some("add" | "remove")) || path.is_none() || parts.next().is_some()
            {
                Err(Error::Config(
                    "usage: /settings skills add|remove DIRECTORY".into(),
                ))
            } else {
                Ok(ConfigChange::SkillDirectory {
                    action: if action == Some("add") {
                        SkillDirectoryAction::Add
                    } else {
                        SkillDirectoryAction::Remove
                    },
                    path: path.unwrap_or_default().to_string(),
                })
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

    let result = change.and_then(|change| agent.change_global_config(change));

    match result {
        Ok(effect) => {
            state.model = agent.model().to_string();
            state.reasoning_effort = agent.config().reasoning_effort.clone();
            state.hide_reasoning = agent.config().hide_reasoning;
            state.accent_color = agent.config().accent_color;
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
        ConfigChangeEffect::Applied => state.notice(format!(
            "Saved to `{}` and applied.",
            config.global_config_path().display()
        )),
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

pub(super) fn show_settings(agent: &Agent, state: &mut ViewState) {
    let mut providers = agent.config().providers.iter().collect::<Vec<_>>();
    providers.sort_by_key(|(name, _)| name.as_str());
    let mut text = format!(
        "Settings\n\n- model: `{}`\n- max_tokens: `{}`\n- reasoning_effort: `{}`\n- hide_reasoning: `{}`\n- accent_color: `{}`\n- auto_compact: `{}`\n- compact_threshold: `{:.0}%`\n- context_window for current model: `{}`\n- anthropic_base_url: `{}`\n- openai_base_url: `{}`\n\nSkill directories\n\n",
        agent.model(),
        agent.config().max_tokens,
        agent
            .config()
            .reasoning_effort
            .as_deref()
            .unwrap_or("provider default"),
        agent.config().hide_reasoning,
        agent.config().accent_color.config_value(),
        if agent.config().auto_compact {
            "on"
        } else {
            "off"
        },
        agent.config().compact_threshold * 100.0,
        agent.context_window(),
        agent.config().anthropic_base_url,
        agent.config().openai_base_url,
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
        "\nChanges are written to `{}`. Project settings in `./.yawl/config.json` override them.\n\nCommands\n\n- `/settings model MODEL`\n- `/settings max_tokens NUMBER`\n- `/settings reasoning_effort default|minimal|low|medium|high|xhigh|max`\n- `/settings hide_reasoning on|off`\n- `/settings accent_color NAME|#RRGGBB`\n- `/settings auto_compact on|off`\n- `/settings compact_threshold 85%`\n- `/settings context_window TOKENS`\n- `/settings skills add|remove DIRECTORY`\n- `/settings provider NAME BASE_URL [API_KEY|-]`\n- `/settings openai_base_url URL`\n- `/settings anthropic_base_url URL`\n- `/settings reload`\n\nUse an environment reference such as `$OMLX_API_KEY` instead of putting a secret directly in terminal history. Pass `-` as the provider key to remove a saved key.",
        agent.config().global_config_path().display()
    ));
    state.notice(text);
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
    let sessions = match crate::session::list(&agent.config().sessions_dir()) {
        Ok(sessions) => sessions,
        Err(error) => {
            state.notice(format!("Could not list sessions: {error}"));
            return;
        }
    };
    if sessions.is_empty() {
        state.notice("No saved sessions.");
        return;
    }
    state.picker = Some(Picker {
        title: "Resume session".into(),
        hint: "↑/↓ move  Enter resume  Esc cancel".into(),
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
                description: session.id.clone(),
                action: PickerAction::ResumeSession(session.id),
            })
            .collect(),
        editing: None,
    });
}

pub(super) fn resume(agent: &mut Agent, selector: &str, state: &mut ViewState) {
    let sessions = match crate::session::list(&agent.config().sessions_dir()) {
        Ok(sessions) => sessions,
        Err(error) => {
            state.notice(format!("Could not list sessions: {error}"));
            return;
        }
    };
    if selector.is_empty() {
        if sessions.is_empty() {
            state.notice("No saved sessions.");
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
