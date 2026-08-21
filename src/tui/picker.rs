//! Picker model, catalogs, and keyboard reducer.

use crate::agent::Agent;
use crate::config::{Config, UiColor};

use super::ViewState;
use super::events::Key;
use super::input::Editor;
use super::markdown;

pub(super) const SETTINGS_REASONING_DISPLAY_INDEX: usize = 3;
pub(super) const SETTINGS_ACCENT_COLOR_INDEX: usize = 4;
pub(super) const SETTINGS_SCROLL_BAR_INDEX: usize = 5;
pub(super) const SETTINGS_AUTO_COMPACT_INDEX: usize = 6;
pub(super) const SETTINGS_RELOAD_INDEX: usize = 13;

#[derive(Clone)]
pub(super) enum PickerAction {
    SwitchModel(String),
    SaveModel(String),
    OpenModels { save: bool },
    OpenReasoning { save: bool },
    SetReasoning { effort: Option<String>, save: bool },
    SetHideReasoning(bool),
    OpenAccentColor,
    SetAccentColor(UiColor),
    SetScrollBar(bool),
    ResumeSession(String),
    EditSetting { key: String, initial: String },
    EditModel { save: bool, initial: String },
    ApplySetting { argument: String, selected: usize },
    SetAutoCompact(bool),
    RemoveQueued(usize),
    ClearQueued,
    Reload,
    ShowSettings,
}

#[derive(Clone)]
pub(super) struct PickerItem {
    pub(super) label: String,
    pub(super) description: String,
    pub(super) action: PickerAction,
}

#[derive(Clone)]
pub(super) struct Picker {
    pub(super) title: String,
    pub(super) hint: String,
    pub(super) items: Vec<PickerItem>,
    pub(super) selected: usize,
    pub(super) editing: Option<PickerEdit>,
}

#[derive(Clone)]
pub(super) enum PickerEdit {
    Setting(String),
    Model { save: bool },
}

pub(super) struct ActivePickers {
    pub(super) model: Picker,
    pub(super) default_model: Picker,
    pub(super) settings: Picker,
    pub(super) reasoning: Picker,
    pub(super) default_reasoning: Picker,
    pub(super) accent_color: Picker,
}

impl ActivePickers {
    pub(super) fn from_agent(agent: &Agent) -> Self {
        Self {
            model: model_picker(agent, false),
            default_model: model_picker(agent, true),
            settings: settings_picker(agent),
            reasoning: reasoning_picker(agent, false),
            default_reasoning: reasoning_picker(agent, true),
            accent_color: color_picker(agent.config().accent_color),
        }
    }

    pub(super) fn refresh_display_settings(&mut self, config: &Config) {
        let visibility = if config.hide_reasoning {
            "Hidden"
        } else {
            "Visible"
        };
        if let Some(item) = self
            .settings
            .items
            .get_mut(SETTINGS_REASONING_DISPLAY_INDEX)
        {
            item.description = format!("{visibility} · Enter to toggle");
            item.action = PickerAction::SetHideReasoning(!config.hide_reasoning);
        }
        if let Some(item) = self.settings.items.get_mut(SETTINGS_ACCENT_COLOR_INDEX) {
            item.description = config.accent_color.config_value();
        }
        if let Some(item) = self.settings.items.get_mut(SETTINGS_SCROLL_BAR_INDEX) {
            let visibility = if config.scroll_bar {
                "Visible"
            } else {
                "Hidden"
            };
            item.description = format!("{visibility} · Enter to toggle");
            item.action = PickerAction::SetScrollBar(!config.scroll_bar);
        }
        self.accent_color = color_picker(config.accent_color);
    }
}

pub(super) fn open_model_picker(agent: &Agent, state: &mut ViewState, save: bool) {
    state.picker = Some(model_picker(agent, save));
}

pub(super) fn model_picker(agent: &Agent, save: bool) -> Picker {
    let selected_model = if save {
        agent.config().model.as_deref().unwrap_or(agent.model())
    } else {
        agent.model()
    };
    let mut models = crate::model::available_models(agent.config());
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
        models.sort_by(|left, right| left.0.cmp(&right.0));
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
        hint: "↑/↓ move  Enter select  Esc cancel".into(),
        items,
        selected,
        editing: None,
    }
}

pub(super) fn open_settings_picker(agent: &Agent, state: &mut ViewState) {
    state.picker = Some(settings_picker(agent));
}

pub(super) fn settings_picker(agent: &Agent) -> Picker {
    let on_off = if agent.config().auto_compact {
        "On"
    } else {
        "Off"
    };
    let reasoning_visibility = if agent.config().hide_reasoning {
        "Hidden"
    } else {
        "Visible"
    };
    let scroll_bar_visibility = if agent.config().scroll_bar {
        "Visible"
    } else {
        "Hidden"
    };
    Picker {
        title: "Settings".into(),
        hint: "↑/↓ move  Enter change  Esc close".into(),
        selected: 0,
        items: vec![
            PickerItem {
                label: "Default model".into(),
                description: agent
                    .config()
                    .model
                    .clone()
                    .unwrap_or_else(|| agent.model().to_string()),
                action: PickerAction::OpenModels { save: true },
            },
            PickerItem {
                label: "Max output tokens".into(),
                description: agent.config().max_tokens.to_string(),
                action: PickerAction::EditSetting {
                    key: "max_tokens".into(),
                    initial: agent.config().max_tokens.to_string(),
                },
            },
            PickerItem {
                label: "Codex reasoning effort".into(),
                description: agent
                    .config()
                    .reasoning_effort
                    .clone()
                    .unwrap_or_else(|| "provider default".into()),
                action: if crate::model::is_codex(agent.config(), agent.model()) {
                    PickerAction::OpenReasoning { save: true }
                } else {
                    PickerAction::EditSetting {
                        key: "reasoning_effort".into(),
                        initial: agent
                            .config()
                            .reasoning_effort
                            .clone()
                            .unwrap_or_else(|| "default".into()),
                    }
                },
            },
            PickerItem {
                label: "Reasoning display".into(),
                description: format!("{reasoning_visibility} · Enter to toggle"),
                action: PickerAction::SetHideReasoning(!agent.config().hide_reasoning),
            },
            PickerItem {
                label: "Accent color".into(),
                description: agent.config().accent_color.config_value(),
                action: PickerAction::OpenAccentColor,
            },
            PickerItem {
                label: "Scroll bar".into(),
                description: format!("{scroll_bar_visibility} · Enter to toggle"),
                action: PickerAction::SetScrollBar(!agent.config().scroll_bar),
            },
            PickerItem {
                label: "Automatic compaction".into(),
                description: format!("{on_off} · Enter to toggle"),
                action: PickerAction::SetAutoCompact(!agent.config().auto_compact),
            },
            PickerItem {
                label: "Compaction threshold".into(),
                description: format!("{:.0}%", agent.config().compact_threshold * 100.0),
                action: PickerAction::EditSetting {
                    key: "compact_threshold".into(),
                    initial: format!("{:.0}%", agent.config().compact_threshold * 100.0),
                },
            },
            PickerItem {
                label: "Current model context window".into(),
                description: agent.context_window().to_string(),
                action: PickerAction::EditSetting {
                    key: "context_window".into(),
                    initial: agent.context_window().to_string(),
                },
            },
            PickerItem {
                label: "Skill directories".into(),
                description: format!(
                    "{} configured · add or remove",
                    agent.config().skill_dirs.len()
                ),
                action: PickerAction::EditSetting {
                    key: "skills".into(),
                    initial: "add ".into(),
                },
            },
            PickerItem {
                label: "OpenAI-compatible provider".into(),
                description: "Add or update a provider".into(),
                action: PickerAction::EditSetting {
                    key: "provider".into(),
                    initial: String::new(),
                },
            },
            PickerItem {
                label: "OpenAI endpoint".into(),
                description: agent.config().openai_base_url.clone(),
                action: PickerAction::EditSetting {
                    key: "openai_base_url".into(),
                    initial: agent.config().openai_base_url.clone(),
                },
            },
            PickerItem {
                label: "Anthropic endpoint".into(),
                description: agent.config().anthropic_base_url.clone(),
                action: PickerAction::EditSetting {
                    key: "anthropic_base_url".into(),
                    initial: agent.config().anthropic_base_url.clone(),
                },
            },
            PickerItem {
                label: "Reload configuration".into(),
                description: "Read global and project files again".into(),
                action: PickerAction::Reload,
            },
            PickerItem {
                label: "Configuration details".into(),
                description: "Show paths, providers, and all commands".into(),
                action: PickerAction::ShowSettings,
            },
        ],
        editing: None,
    }
}

pub(super) fn color_picker(current: UiColor) -> Picker {
    let choices = [
        ("White", UiColor::WHITE),
        ("Gray", UiColor::new(148, 148, 158)),
        ("Red", UiColor::new(235, 111, 146)),
        ("Orange", UiColor::new(240, 160, 96)),
        ("Yellow", UiColor::new(232, 202, 118)),
        ("Green", UiColor::new(139, 213, 162)),
        ("Cyan", UiColor::new(116, 199, 213)),
        ("Blue", UiColor::new(117, 169, 255)),
        ("Purple", UiColor::new(190, 149, 255)),
        ("Pink", UiColor::new(238, 148, 200)),
    ];
    let mut items = choices
        .into_iter()
        .map(|(label, color)| PickerItem {
            label: label.into(),
            description: format!(
                "\x1b[48;2;{};{};{}m   \x1b[0m {}",
                color.red,
                color.green,
                color.blue,
                color.config_value()
            ),
            action: PickerAction::SetAccentColor(color),
        })
        .collect::<Vec<_>>();
    items.push(PickerItem {
        label: "Custom RGB…".into(),
        description: "Enter #RRGGBB".into(),
        action: PickerAction::EditSetting {
            key: "accent_color".into(),
            initial: current.config_value(),
        },
    });
    let selected = items
        .iter()
        .position(|item| {
            matches!(
                item.action,
                PickerAction::SetAccentColor(color) if color == current
            )
        })
        .unwrap_or(items.len().saturating_sub(1));
    Picker {
        title: "Accent color".into(),
        hint: "↑/↓ move  Enter select  Esc cancel".into(),
        items,
        selected,
        editing: None,
    }
}

pub(super) fn open_reasoning_picker(agent: &Agent, state: &mut ViewState, save: bool) {
    state.picker = Some(reasoning_picker(agent, save));
}

pub(super) fn reasoning_picker(agent: &Agent, save: bool) -> Picker {
    let target = crate::model::ModelTarget::parse(agent.model(), agent.config());
    let model = target.model();
    let current = agent.config().reasoning_effort.as_deref();
    let mut items = vec![PickerItem {
        label: "Provider default".into(),
        description: "Do not request a specific effort".into(),
        action: PickerAction::SetReasoning { effort: None, save },
    }];
    items.extend(
        crate::model::reasoning_efforts(agent.config(), agent.model())
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
    }
}

pub(super) fn title_case_effort(effort: &str) -> String {
    let mut chars = effort.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

pub(super) fn reasoning_description(effort: &str) -> &'static str {
    match effort {
        "minimal" => "Fastest, least deliberation",
        "low" => "Fast with light deliberation",
        "medium" => "Balanced speed and depth",
        "high" => "More thorough reasoning",
        "xhigh" => "Very thorough reasoning",
        "max" => "Maximum available reasoning",
        _ => "",
    }
}

pub(super) fn picker_is_editing(state: &ViewState) -> bool {
    state
        .picker
        .as_ref()
        .is_some_and(|picker| picker.editing.is_some())
}

pub(super) fn take_picker_action(
    state: &mut ViewState,
    editor: &mut Editor,
    key: Key,
) -> Option<PickerAction> {
    let picker = state.picker.as_mut()?;

    if let Some(editing) = picker.editing.clone() {
        match key {
            Key::Escape | Key::Ctrl('c') => {
                editor.clear();
                picker.editing = None;
            }
            Key::Enter => {
                if let Some(value) = editor.take_text() {
                    let selected = match &editing {
                        PickerEdit::Setting(key) if key == "accent_color" => {
                            SETTINGS_ACCENT_COLOR_INDEX
                        }
                        _ => picker.selected,
                    };
                    state.picker = None;
                    return Some(match editing {
                        PickerEdit::Setting(key) => PickerAction::ApplySetting {
                            argument: format!("{key} {}", value.trim()),
                            selected,
                        },
                        PickerEdit::Model { save } => {
                            if save {
                                PickerAction::SaveModel(value.trim().to_string())
                            } else {
                                PickerAction::SwitchModel(value.trim().to_string())
                            }
                        }
                    });
                }
            }
            Key::Up | Key::Down => {}
            _ => {
                let _ = editor.handle_key(key);
            }
        }
        return None;
    }

    match key {
        Key::Escape | Key::Ctrl('c') => state.picker = None,
        Key::Up | Key::Char('k') => picker.selected = picker.selected.saturating_sub(1),
        Key::Down | Key::Char('j') => {
            picker.selected = (picker.selected + 1).min(picker.items.len().saturating_sub(1));
        }
        Key::PageUp => picker.selected = picker.selected.saturating_sub(5),
        Key::PageDown => {
            picker.selected = (picker.selected + 5).min(picker.items.len().saturating_sub(1));
        }
        Key::Enter => {
            let action = picker
                .items
                .get(picker.selected)
                .map(|item| item.action.clone());
            match action {
                Some(PickerAction::EditSetting { key, initial }) => {
                    editor.clear();
                    editor.paste(&initial);
                    picker.editing = Some(PickerEdit::Setting(key));
                }
                Some(PickerAction::EditModel { save, initial }) => {
                    editor.clear();
                    editor.paste(&initial);
                    picker.editing = Some(PickerEdit::Model { save });
                }
                Some(action) => {
                    state.picker = None;
                    return Some(action);
                }
                None => {}
            }
        }
        _ => {}
    }
    None
}

pub(super) fn select_picker_item(state: &mut ViewState, selected: usize) {
    if let Some(picker) = state.picker.as_mut() {
        picker.selected = selected.min(picker.items.len().saturating_sub(1));
    }
}

pub(super) fn render_picker(
    picker: &Picker,
    editor: &Editor,
    columns: usize,
    height: usize,
) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    let box_width = columns.saturating_sub(4).clamp(16, 76);
    let inner = box_width.saturating_sub(2);
    let capacity = height
        .saturating_sub(5)
        .max(1)
        .min(picker.items.len().max(1));
    let mut start = picker.selected.saturating_sub(capacity / 2);
    start = start.min(picker.items.len().saturating_sub(capacity));
    let end = (start + capacity).min(picker.items.len());
    let left = " ".repeat(columns.saturating_sub(box_width) / 2);
    let boxed = |content: &str| format!("{left}│{}│", markdown::fit_width(content, inner));
    let mut panel = vec![format!("{left}┌{}┐", "─".repeat(inner))];
    panel.push(boxed(&format!(" \x1b[1m{}\x1b[0m", picker.title)));
    panel.push(format!("{left}├{}┤", "─".repeat(inner)));
    for (index, item) in picker.items[start..end].iter().enumerate() {
        let absolute = start + index;
        let marker = if absolute == picker.selected {
            "›"
        } else {
            " "
        };
        let description = if absolute == picker.selected && picker.editing.is_some() {
            let value = editor.text();
            if value.is_empty() {
                "type a value below…".into()
            } else {
                value.replace('\n', " ")
            }
        } else {
            item.description.clone()
        };
        let text = format!(" {marker} {}  ·  {description}", item.label);
        if absolute == picker.selected {
            panel.push(boxed(&format!(
                "\x1b[7m{}\x1b[0m",
                markdown::fit_width(&text, inner)
            )));
        } else {
            panel.push(boxed(&text));
        }
    }
    let hint = if picker.editing.is_some() {
        "Edit below  Enter save  Esc cancel edit"
    } else {
        &picker.hint
    };
    panel.push(boxed(&format!(" \x1b[2m{hint}\x1b[0m")));
    panel.push(format!("{left}└{}┘", "─".repeat(inner)));

    if panel.len() > height {
        panel.truncate(height);
    }
    let top = height.saturating_sub(panel.len()) / 2;
    let mut lines = Vec::with_capacity(height);
    lines.extend(std::iter::repeat_n(" ".repeat(columns), top));
    lines.extend(
        panel
            .into_iter()
            .map(|line| markdown::fit_width(&line, columns)),
    );
    lines.extend(std::iter::repeat_n(
        " ".repeat(columns),
        height.saturating_sub(lines.len()),
    ));
    lines
}
