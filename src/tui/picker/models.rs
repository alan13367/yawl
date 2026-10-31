//! Model and reasoning-effort pickers, including the background refresh of
//! the Codex model catalog while a model picker is open.

use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::agent::Agent;
use crate::config::Config;

use super::super::ViewState;
use super::{
    Picker, PickerAction, PickerItem, SettingsCategory, SettingsItem, SettingsLocation,
    settings_item_index,
};

pub(in crate::tui) struct ModelRefreshJob {
    receiver: Receiver<Result<(), crate::error::Error>>,
    config: Config,
    selected_model: String,
    save: bool,
    subagent: bool,
}

pub(in crate::tui) fn open_model_picker(agent: &Agent, state: &mut ViewState, save: bool) {
    let selected_model = if save {
        agent.config().model.as_deref().unwrap_or(agent.model())
    } else {
        agent.model()
    };
    open_model_picker_from_config(agent.config(), selected_model, state, save);
}

pub(in crate::tui) fn open_model_picker_from_config(
    config: &Config,
    selected_model: &str,
    state: &mut ViewState,
    save: bool,
) {
    state.picker = Some(model_picker(config, selected_model, save));
    start_model_refresh(config, selected_model, state, save, false);
}

pub(in crate::tui) fn open_subagent_model_picker(config: &Config, state: &mut ViewState) {
    state.picker = Some(subagent_model_picker(config, &config.subagent_model));
    start_model_refresh(config, &config.subagent_model, state, true, true);
}

fn start_model_refresh(
    config: &Config,
    selected_model: &str,
    state: &mut ViewState,
    save: bool,
    subagent: bool,
) {
    state.model_refresh = None;
    if crate::provider::codex::credential_status(config)
        != crate::provider::codex::CodexLoginStatus::LoggedIn
    {
        return;
    }
    if let Some(picker) = state.picker.as_mut() {
        picker.hint.insert_str(0, REFRESHING_HINT);
    }
    let config_for_worker = config.clone();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let result = crate::provider::codex::refresh_catalog(&config_for_worker).map(|_| ());
        let _ = sender.send(result);
    });
    state.model_refresh = Some(ModelRefreshJob {
        receiver,
        config: config.clone(),
        selected_model: selected_model.to_string(),
        save,
        subagent,
    });
}

pub(in crate::tui) fn poll_model_picker(state: &mut ViewState) -> bool {
    let Some(job) = state.model_refresh.take() else {
        return false;
    };
    let expected_title = if job.subagent {
        "Default subagent model"
    } else if job.save {
        "Default model"
    } else {
        "Choose model"
    };
    let Some(picker) = state
        .picker
        .as_ref()
        .filter(|picker| picker.title == expected_title)
    else {
        return false;
    };
    match job.receiver.try_recv() {
        Err(TryRecvError::Empty) => {
            state.model_refresh = Some(job);
            false
        }
        Ok(Ok(())) => {
            if picker.editing.is_none() {
                state.picker = Some(if job.subagent {
                    refreshed_subagent_model_picker(&job.config, &job.selected_model, picker)
                } else {
                    refreshed_model_picker(&job.config, &job.selected_model, job.save, picker)
                });
                true
            } else {
                clear_refreshing_hint(state);
                true
            }
        }
        Ok(Err(_)) | Err(TryRecvError::Disconnected) => {
            state.notice("Could not refresh Codex models. Showing the saved catalog.");
            clear_refreshing_hint(state);
            true
        }
    }
}

const REFRESHING_HINT: &str = "Refreshing Codex models…  ";

fn clear_refreshing_hint(state: &mut ViewState) {
    if let Some(picker) = state.picker.as_mut()
        && let Some(hint) = picker.hint.strip_prefix(REFRESHING_HINT)
    {
        picker.hint = hint.to_string();
    }
}

pub(in crate::tui) fn refreshed_model_picker(
    config: &Config,
    selected_model: &str,
    save: bool,
    previous: &Picker,
) -> Picker {
    refresh_model_selection(previous, model_picker(config, selected_model, save))
}

pub(in crate::tui) fn refreshed_subagent_model_picker(
    config: &Config,
    selected_model: &str,
    previous: &Picker,
) -> Picker {
    refresh_model_selection(previous, subagent_model_picker(config, selected_model))
}

fn refresh_model_selection(previous: &Picker, mut updated: Picker) -> Picker {
    let selected = previous
        .items
        .get(previous.selected)
        .map(|item| &item.action);
    if let Some(index) = updated
        .items
        .iter()
        .position(|item| match (selected, &item.action) {
            (Some(PickerAction::SaveModel(old)), PickerAction::SaveModel(new))
            | (Some(PickerAction::SwitchModel(old)), PickerAction::SwitchModel(new)) => old == new,
            (
                Some(PickerAction::EditModel { save: old, .. }),
                PickerAction::EditModel { save: new, .. },
            ) => old == new,
            (
                Some(PickerAction::ApplySetting { argument: old, .. }),
                PickerAction::ApplySetting { argument: new, .. },
            ) => old == new,
            (
                Some(PickerAction::EditSetting { key: old, .. }),
                PickerAction::EditSetting { key: new, .. },
            ) => old == new,
            _ => false,
        })
    {
        updated.selected = index;
    }
    updated
}

pub(in crate::tui) fn subagent_model_picker(config: &Config, selected_model: &str) -> Picker {
    let location = Some(SettingsLocation {
        category: SettingsCategory::Subagents,
        item: SettingsItem::SubagentModel,
    });
    let mut models = crate::model::available_models(config);
    if selected_model != "inherit" && !models.iter().any(|(id, _)| id == selected_model) {
        models.push((selected_model.to_string(), "Current default".into()));
    }
    let mut items = vec![PickerItem {
        label: "Inherit".into(),
        description: "Use the active parent model".into(),
        action: PickerAction::ApplySetting {
            argument: "subagent_model inherit".into(),
            location,
        },
    }];
    items.extend(models.into_iter().map(|(id, label)| PickerItem {
        label,
        description: id.clone(),
        action: PickerAction::ApplySetting {
            argument: format!("subagent_model {id}"),
            location,
        },
    }));
    items.push(PickerItem {
        label: "Use another model ID…".into(),
        description: "Enter a model not listed above".into(),
        action: PickerAction::EditSetting {
            key: "subagent_model".into(),
            initial: if selected_model == "inherit" {
                String::new()
            } else {
                selected_model.to_string()
            },
            location,
        },
    });
    let argument = format!("subagent_model {selected_model}");
    let selected = items
        .iter()
        .position(|item| {
            matches!(&item.action, PickerAction::ApplySetting { argument: value, .. } if value == &argument)
        })
        .unwrap_or(0);
    Picker {
        title: "Default subagent model".into(),
        hint: "↑/↓ move  Enter select  Esc cancel".into(),
        items,
        selected,
        editing: None,
        filter: None,
        parent: Some(PickerAction::OpenSettingsCategory {
            category: SettingsCategory::Subagents,
            selected: settings_item_index(SettingsCategory::Subagents, SettingsItem::SubagentModel),
        }),
    }
}

pub(in crate::tui) fn model_picker(config: &Config, selected_model: &str, save: bool) -> Picker {
    let mut models = crate::model::available_models(config);
    if !models.iter().any(|(model, _)| model == selected_model) {
        models.push((
            selected_model.to_string(),
            if save {
                "Current default"
            } else {
                "Current model"
            }
            .into(),
        ));
    }
    let mut items = models
        .into_iter()
        .map(|(model, name)| PickerItem {
            label: name,
            description: model.clone(),
            action: if save {
                PickerAction::SaveModel(model)
            } else {
                PickerAction::SwitchModel(model)
            },
        })
        .collect::<Vec<_>>();
    items.push(PickerItem {
        label: "Use another model ID…".into(),
        description: "Enter a model not listed above".into(),
        action: PickerAction::EditModel {
            save,
            initial: String::new(),
        },
    });
    let selected = items
        .iter()
        .position(|item| item.description == selected_model)
        .unwrap_or(0);
    Picker {
        title: if save {
            "Default model".into()
        } else {
            "Choose model".into()
        },
        hint: "↑/↓ move  Enter select  d remove  Esc cancel".into(),
        items,
        selected,
        editing: None,
        filter: None,
        parent: save.then_some(PickerAction::OpenSettingsCategory {
            category: SettingsCategory::Model,
            selected: settings_item_index(SettingsCategory::Model, SettingsItem::DefaultModel),
        }),
    }
}

pub(in crate::tui) fn open_reasoning_picker(agent: &Agent, state: &mut ViewState, save: bool) {
    state.picker = Some(reasoning_picker(agent, save));
}

pub(in crate::tui) fn reasoning_picker(agent: &Agent, save: bool) -> Picker {
    reasoning_picker_from_config(agent.config(), agent.model(), save)
}

pub(in crate::tui) fn reasoning_picker_from_config(
    config: &Config,
    model_spec: &str,
    save: bool,
) -> Picker {
    let target = crate::model::ModelTarget::parse(model_spec, config);
    let model = target.model();
    let current = config.reasoning_effort.as_deref();
    let mut items = vec![PickerItem {
        label: "Provider default".into(),
        description: "Do not request a specific effort".into(),
        action: PickerAction::SetReasoning { effort: None, save },
    }];
    items.extend(
        crate::model::reasoning_efforts(config, model_spec)
            .iter()
            .map(|effort| PickerItem {
                label: title_case_effort(effort),
                description: reasoning_description(effort).into(),
                action: PickerAction::SetReasoning {
                    effort: Some((*effort).to_string()),
                    save,
                },
            }),
    );
    let selected = current
        .and_then(|current| {
            items.iter().position(|item| {
                matches!(
                    &item.action,
                    PickerAction::SetReasoning { effort: Some(effort), .. } if effort == current
                )
            })
        })
        .unwrap_or(0);
    Picker {
        title: format!("Reasoning · {model}"),
        hint: "↑/↓ move  Enter select  Esc cancel".into(),
        items,
        selected,
        editing: None,
        filter: None,
        parent: save.then_some(PickerAction::OpenSettingsCategory {
            category: SettingsCategory::Model,
            selected: settings_item_index(SettingsCategory::Model, SettingsItem::ReasoningEffort),
        }),
    }
}

fn title_case_effort(effort: &str) -> String {
    let mut chars = effort.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

fn reasoning_description(effort: &str) -> &'static str {
    match effort {
        "minimal" => "Fastest, least deliberation",
        "low" => "Fast with light deliberation",
        "medium" => "Balanced speed and depth",
        "high" => "More thorough reasoning",
        "xhigh" => "Very thorough reasoning",
        "max" => "Maximum available reasoning",
        "ultra" => "Ultra reasoning effort",
        _ => "",
    }
}
