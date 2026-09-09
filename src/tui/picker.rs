//! Picker model, catalogs, and keyboard reducer.
//!
//! This facade owns the shared picker model and keyboard reducer. Private
//! children build the settings and status-bar editor catalogs. TUI callers
//! keep the entry points here.

mod settings;
mod status_bar;

pub(super) use settings::{
    color_picker, open_settings_picker, selection_color_picker, settings_category_picker,
    settings_category_picker_from, settings_item_index, settings_picker,
    web_search_provider_picker,
};
pub(super) use status_bar::{
    status_bar_add_picker, status_bar_editor_picker, status_bar_format_picker,
    status_bar_item_picker, status_bar_separator_picker, status_bar_style_picker,
};

use crate::agent::Agent;
use crate::config::{
    Config, StatusBarFormat, StatusBarKind, StatusBarStyle, StatusBarVisibility, UiColor,
    WebSearchProvider,
};

use super::ViewState;
use super::connection::{ConnectEditField, ConnectStep};
use super::events::Key;
use super::input::Editor;
use super::markdown;
use crate::onboarding::provider::{
    ConnectionActivation, ConnectionPlan, CredentialChoice, ProviderId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsCategory {
    Model,
    Interface,
    Context,
    Providers,
    Web,
    Subagents,
    Skills,
    Advanced,
}

impl SettingsCategory {
    pub(super) const ALL: [Self; 8] = [
        Self::Model,
        Self::Interface,
        Self::Context,
        Self::Providers,
        Self::Web,
        Self::Subagents,
        Self::Skills,
        Self::Advanced,
    ];

    pub(super) const fn title(self) -> &'static str {
        match self {
            Self::Model => "Model",
            Self::Interface => "Interface",
            Self::Context => "Context",
            Self::Providers => "Providers",
            Self::Web => "Web",
            Self::Subagents => "Subagents",
            Self::Skills => "Skills",
            Self::Advanced => "Advanced",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::Model => "Default model, output, and reasoning",
            Self::Interface => "Colors, reasoning display, scroll bar, and bell",
            Self::Context => "Compaction and context windows",
            Self::Providers => "Add or update model providers",
            Self::Web => "Browsing, search source, fetch limit, and keys",
            Self::Subagents => "Concurrency, models, budgets, and timeouts",
            Self::Skills => "Search directories for reusable skills",
            Self::Advanced => "Reload and inspect configuration",
        }
    }

    pub(super) fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|category| *category == self)
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsItem {
    DefaultModel,
    MaxOutputTokens,
    ReasoningEffort,
    ReasoningDisplay,
    AccentColor,
    SelectionColor,
    StatusBar,
    ScrollBar,
    ScrollBarAutoHide,
    Bell,
    AutoCompact,
    CompactThreshold,
    ContextWindow,
    ProviderSetup,
    WebBrowsingEnabled,
    WebSearchProvider,
    WebFetchMaxChars,
    BraveApiKey,
    FirecrawlApiKey,
    SubagentsEnabled,
    MaxSubagents,
    SubagentModel,
    SubagentRequestBudget,
    SubagentTimeout,
    AddSkillDirectory,
    Reload,
    ConfigurationDetails,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SettingsLocation {
    pub(super) category: SettingsCategory,
    pub(super) item: SettingsItem,
}

#[derive(Clone)]
pub(super) enum PickerAction {
    SwitchModel(String),
    SaveModel(String),
    OpenModels {
        save: bool,
    },
    OpenReasoning {
        save: bool,
    },
    SetReasoning {
        effort: Option<String>,
        save: bool,
    },
    SetHideReasoning(bool),
    OpenAccentColor,
    SetAccentColor(UiColor),
    OpenSelectionColor,
    /// `None` follows the accent color.
    SetSelectionColor(Option<UiColor>),
    OpenStatusBarEditor {
        selected: usize,
    },
    CancelStatusBarEditor,
    OpenStatusBarItem {
        index: usize,
        selected: usize,
    },
    OpenStatusBarFormats(usize),
    SetStatusBarFormat {
        index: usize,
        format: StatusBarFormat,
    },
    SetStatusBarVisibility {
        index: usize,
        visibility: StatusBarVisibility,
    },
    EditStatusBarLabel {
        index: usize,
        initial: String,
    },
    ApplyStatusBarLabel {
        index: usize,
        label: String,
    },
    ResetStatusBarLabel(usize),
    OpenStatusBarAdd,
    AddStatusBarItem(StatusBarKind),
    MoveStatusBarItem {
        index: usize,
        direction: isize,
    },
    RemoveStatusBarItem(usize),
    OpenStatusBarStyles,
    SetStatusBarStyle(StatusBarStyle),
    OpenStatusBarSeparators,
    SetStatusBarSeparator(String),
    EditStatusBarSeparator(String),
    ApplyStatusBarSeparator(String),
    ResetStatusBar,
    SaveStatusBar,
    OpenWebSearchProviders,
    SetWebSearchProvider(WebSearchProvider),
    SetScrollBar(bool),
    SetScrollBarAutoHide(bool),
    SetBell(bool),
    ResumeSession(String),
    DeleteSession(String),
    /// Reopen the resume picker, restoring `selected` after a canceled delete.
    OpenResume {
        selected: usize,
    },
    OpenSettingsRoot {
        selected: usize,
    },
    OpenSettingsCategory {
        category: SettingsCategory,
        selected: usize,
    },
    OpenConnect {
        from_settings: bool,
    },
    ConnectChooseProvider(ProviderId),
    EditConnect {
        field: ConnectEditField,
        initial: String,
        secret: bool,
    },
    ApplyConnect {
        field: ConnectEditField,
        value: String,
    },
    ConnectCredential(CredentialChoice),
    ConnectChooseModel(String),
    ConnectRetry,
    ConnectCancelJob,
    CloseConnect,
    ConnectBack(ConnectStep),
    ConnectActivation(ConnectionActivation),
    ApplyConnectionPlan(ConnectionPlan),
    EditSetting {
        key: String,
        initial: String,
        location: Option<SettingsLocation>,
    },
    EditSecretSetting {
        key: String,
        location: Option<SettingsLocation>,
    },
    EditModel {
        save: bool,
        initial: String,
    },
    ApplySetting {
        argument: String,
        location: Option<SettingsLocation>,
    },
    SetAutoCompact(bool),
    SetWebBrowsing(bool),
    SetSubagents(bool),
    SendQueued(usize),
    ApplyQueued {
        index: usize,
        value: String,
    },
    MoveQueued {
        index: usize,
        direction: isize,
    },
    RemoveQueued(usize),
    ClearQueued,
    ImplementPlan,
    ReturnFromPlan,
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
    /// Action performed by Escape. Standalone pickers leave this unset.
    pub(super) parent: Option<PickerAction>,
}

#[derive(Clone)]
pub(super) enum PickerEdit {
    Setting(String),
    SecretSetting(String),
    Model {
        save: bool,
    },
    Queued(usize),
    StatusBarLabel(usize),
    StatusBarSeparator,
    Connect {
        field: ConnectEditField,
        secret: bool,
    },
}

pub(super) struct ActivePickers {
    pub(super) model: Picker,
    pub(super) default_model: Picker,
    pub(super) settings: Picker,
    pub(super) settings_categories: Vec<(SettingsCategory, Picker)>,
    pub(super) reasoning: Picker,
    pub(super) default_reasoning: Picker,
    pub(super) accent_color: Picker,
    pub(super) selection_color: Picker,
}

impl ActivePickers {
    pub(super) fn from_agent(agent: &Agent) -> Self {
        Self {
            model: model_picker(agent, false),
            default_model: model_picker(agent, true),
            settings: settings_picker(agent),
            settings_categories: SettingsCategory::ALL
                .into_iter()
                .map(|category| (category, settings_category_picker(agent, category, 0)))
                .collect(),
            reasoning: reasoning_picker(agent, false),
            default_reasoning: reasoning_picker(agent, true),
            accent_color: color_picker(agent.config().accent_color),
            selection_color: selection_color_picker(agent.config().selection_color),
        }
    }

    pub(super) fn refresh_display_settings(&mut self, config: &Config) {
        if let Some((_, picker)) = self
            .settings_categories
            .iter_mut()
            .find(|(category, _)| *category == SettingsCategory::Interface)
        {
            *picker = settings_category_picker_from(
                config,
                "",
                0,
                SettingsCategory::Interface,
                picker.selected,
            );
        }
        self.accent_color = color_picker(config.accent_color);
        self.selection_color = selection_color_picker(config.selection_color);
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
        parent: save.then_some(PickerAction::OpenSettingsCategory {
            category: SettingsCategory::Model,
            selected: settings_item_index(SettingsCategory::Model, SettingsItem::DefaultModel),
        }),
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
        parent: save.then_some(PickerAction::OpenSettingsCategory {
            category: SettingsCategory::Model,
            selected: settings_item_index(SettingsCategory::Model, SettingsItem::ReasoningEffort),
        }),
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

pub(super) fn picker_is_secret(state: &ViewState) -> bool {
    state.picker.as_ref().is_some_and(|picker| {
        matches!(
            picker.editing,
            Some(PickerEdit::Connect { secret: true, .. } | PickerEdit::SecretSetting(_))
        )
    })
}

pub(super) fn picker_is_plan_handoff(picker: &Picker) -> bool {
    matches!(
        picker.items.first().map(|item| &item.action),
        Some(PickerAction::ImplementPlan)
    ) && matches!(
        picker.items.get(1).map(|item| &item.action),
        Some(PickerAction::ReturnFromPlan)
    )
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
                match editing {
                    PickerEdit::StatusBarLabel(index) => {
                        let value = editor.text();
                        editor.clear();
                        state.picker = None;
                        return Some(PickerAction::ApplyStatusBarLabel {
                            index,
                            label: value,
                        });
                    }
                    PickerEdit::StatusBarSeparator => {
                        let value = editor.text();
                        editor.clear();
                        state.picker = None;
                        return Some(PickerAction::ApplyStatusBarSeparator(value));
                    }
                    _ => {}
                }
                if let Some(value) = editor.take_text() {
                    let location =
                        picker
                            .items
                            .get(picker.selected)
                            .and_then(|item| match &item.action {
                                PickerAction::EditSetting { location, .. } => *location,
                                PickerAction::EditSecretSetting { location, .. } => *location,
                                _ => None,
                            });
                    state.picker = None;
                    return Some(match editing {
                        PickerEdit::Setting(key) => PickerAction::ApplySetting {
                            argument: format!("{key} {}", value.trim()),
                            location,
                        },
                        PickerEdit::SecretSetting(key) => PickerAction::ApplySetting {
                            argument: format!("{key} {}", value.trim()),
                            location,
                        },
                        PickerEdit::Model { save } => {
                            if save {
                                PickerAction::SaveModel(value.trim().to_string())
                            } else {
                                PickerAction::SwitchModel(value.trim().to_string())
                            }
                        }
                        PickerEdit::Queued(index) => PickerAction::ApplyQueued { index, value },
                        PickerEdit::StatusBarLabel(_) | PickerEdit::StatusBarSeparator => {
                            unreachable!("status-bar edits return before non-empty edit handling")
                        }
                        PickerEdit::Connect { field, .. } => PickerAction::ApplyConnect {
                            field,
                            value: value.trim().to_string(),
                        },
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

    if picker_is_plan_handoff(picker)
        && let Key::Char(digit @ '1'..='2') = key
    {
        picker.selected = usize::from(digit as u8 - b'1');
        return None;
    }

    match key {
        Key::Escape | Key::Ctrl('c') => {
            let cancel = picker_cancel_action(picker);
            state.picker = None;
            return cancel;
        }
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
                Some(PickerAction::EditSetting { key, initial, .. }) => {
                    editor.clear();
                    editor.paste(&initial);
                    picker.editing = Some(PickerEdit::Setting(key));
                }
                Some(PickerAction::EditSecretSetting { key, .. }) => {
                    editor.clear();
                    picker.editing = Some(PickerEdit::SecretSetting(key));
                }
                Some(PickerAction::EditModel { save, initial }) => {
                    editor.clear();
                    editor.paste(&initial);
                    picker.editing = Some(PickerEdit::Model { save });
                }
                Some(PickerAction::EditConnect {
                    field,
                    initial,
                    secret,
                }) => {
                    editor.clear();
                    editor.paste(&initial);
                    picker.editing = Some(PickerEdit::Connect { field, secret });
                }
                Some(PickerAction::EditStatusBarLabel { index, initial }) => {
                    editor.clear();
                    editor.paste(&initial);
                    picker.editing = Some(PickerEdit::StatusBarLabel(index));
                }
                Some(PickerAction::EditStatusBarSeparator(initial)) => {
                    editor.clear();
                    editor.paste(&initial);
                    picker.editing = Some(PickerEdit::StatusBarSeparator);
                }
                Some(action) => {
                    state.picker = None;
                    return Some(action);
                }
                None => {}
            }
        }
        Key::Char('e') => {
            if let Some(PickerAction::SendQueued(index)) = picker
                .items
                .get(picker.selected)
                .map(|item| item.action.clone())
                && let Some(initial) = state.queued_inputs.get(index)
            {
                editor.clear();
                editor.restore_submission(initial.clone());
                picker.editing = Some(PickerEdit::Queued(index));
            }
        }
        Key::Char('K') | Key::Char('J') => {
            let action = picker
                .items
                .get(picker.selected)
                .map(|item| item.action.clone());
            match action {
                Some(PickerAction::SendQueued(index)) => {
                    state.picker = None;
                    return Some(PickerAction::MoveQueued {
                        index,
                        direction: if key == Key::Char('K') { -1 } else { 1 },
                    });
                }
                Some(PickerAction::OpenStatusBarItem { index, .. }) => {
                    state.picker = None;
                    return Some(PickerAction::MoveStatusBarItem {
                        index,
                        direction: if key == Key::Char('K') { -1 } else { 1 },
                    });
                }
                _ => {}
            }
        }
        Key::Delete | Key::Char('d') => {
            let selected = picker.selected;
            let action = picker.items.get(selected).map(|item| item.action.clone());
            match action {
                Some(PickerAction::ResumeSession(id)) => {
                    let label = picker.items[selected].label.clone();
                    let description = picker.items[selected].description.clone();
                    *picker = delete_session_confirm(id, label, description, selected);
                }
                Some(PickerAction::SendQueued(index)) => {
                    return Some(PickerAction::RemoveQueued(index));
                }
                Some(PickerAction::OpenStatusBarItem { index, .. }) => {
                    return Some(PickerAction::RemoveStatusBarItem(index));
                }
                _ => {}
            }
        }
        _ => {}
    }
    None
}

fn delete_session_confirm(
    id: String,
    label: String,
    description: String,
    resume_selected: usize,
) -> Picker {
    Picker {
        title: "Delete session?".into(),
        hint: "Enter confirm  Esc back".into(),
        selected: 0,
        items: vec![
            PickerItem {
                label: "Cancel".into(),
                description: "Keep this session".into(),
                action: PickerAction::OpenResume {
                    selected: resume_selected,
                },
            },
            PickerItem {
                label: "Delete".into(),
                description: format!("{label} · {description}"),
                action: PickerAction::DeleteSession(id),
            },
        ],
        editing: None,
        parent: Some(PickerAction::OpenResume {
            selected: resume_selected,
        }),
    }
}

fn picker_cancel_action(picker: &Picker) -> Option<PickerAction> {
    picker.parent.clone()
}

pub(super) fn select_picker_item(state: &mut ViewState, selected: usize) {
    if let Some(picker) = state.picker.as_mut() {
        picker.selected = selected.min(picker.items.len().saturating_sub(1));
    }
}

pub(super) fn render_picker(
    picker: &Picker,
    editor: &Editor,
    selection: &str,
    outline: &str,
    columns: usize,
    height: usize,
) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    let box_width = columns.saturating_sub(4).max(16).min(columns);
    let inner = box_width.saturating_sub(2);
    let capacity = height
        .saturating_sub(5)
        .max(1)
        .min(picker.items.len().max(1));
    let mut start = picker.selected.saturating_sub(capacity / 2);
    start = start.min(picker.items.len().saturating_sub(capacity));
    let end = (start + capacity).min(picker.items.len());
    let left = " ".repeat(columns.saturating_sub(box_width) / 2);
    let boxed = |content: &str| {
        format!(
            "{left}{outline}│\x1b[0m{}{outline}│\x1b[0m",
            markdown::fit_width(content, inner)
        )
    };
    let mut panel = vec![format!("{left}{outline}┌{}┐\x1b[0m", "─".repeat(inner))];
    panel.push(boxed(&format!(" \x1b[1m{}\x1b[0m", picker.title)));
    panel.push(format!("{left}{outline}├{}┤\x1b[0m", "─".repeat(inner)));
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
            } else if matches!(
                picker.editing,
                Some(PickerEdit::Connect { secret: true, .. } | PickerEdit::SecretSetting(_))
            ) {
                "•".repeat(value.chars().count())
            } else {
                value.replace('\n', " ")
            }
        } else {
            item.description.clone()
        };
        let text = format!(" {marker} {}  ·  {description}", item.label);
        if absolute == picker.selected {
            panel.push(boxed(&super::render::selected_row(
                &markdown::fit_width(&text, inner),
                selection,
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
    panel.push(format!("{left}{outline}└{}┘\x1b[0m", "─".repeat(inner)));

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
