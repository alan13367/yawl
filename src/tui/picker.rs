//! Picker model, catalogs, and keyboard reducer.

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
            Self::Interface => "Colors, reasoning display, and scroll bar",
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

/// Named colors offered by the accent and selection pickers, mirroring
/// `UiColor::parse`.
const COLOR_PALETTE: [(&str, UiColor); 10] = [
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

fn swatch_description(color: UiColor) -> String {
    format!(
        "\x1b[48;2;{};{};{}m   \x1b[0m {}",
        color.red,
        color.green,
        color.blue,
        color.config_value()
    )
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

pub(super) fn open_settings_picker(agent: &Agent, state: &mut ViewState) {
    state.picker = Some(settings_picker(agent));
}

pub(super) fn settings_picker(_agent: &Agent) -> Picker {
    Picker {
        title: "Settings".into(),
        hint: "↑/↓ move  Enter open  Esc close".into(),
        selected: 0,
        items: SettingsCategory::ALL
            .into_iter()
            .map(|category| PickerItem {
                label: category.title().into(),
                description: category.description().into(),
                action: PickerAction::OpenSettingsCategory {
                    category,
                    selected: 0,
                },
            })
            .collect(),
        editing: None,
        parent: None,
    }
}

pub(super) fn settings_category_picker(
    agent: &Agent,
    category: SettingsCategory,
    selected: usize,
) -> Picker {
    settings_category_picker_from(
        agent.config(),
        agent.model(),
        agent.context_window(),
        category,
        selected,
    )
}

pub(super) fn settings_category_picker_from(
    config: &Config,
    model: &str,
    context_window: u64,
    category: SettingsCategory,
    selected: usize,
) -> Picker {
    let location = |item| Some(SettingsLocation { category, item });
    let edit = |item, label: &str, value: String| PickerItem {
        label: label.into(),
        description: value.clone(),
        action: PickerAction::EditSetting {
            key: setting_key(item).into(),
            initial: value,
            location: location(item),
        },
    };
    let on_off = |enabled| if enabled { "On" } else { "Off" };
    let mut items = match category {
        SettingsCategory::Model => vec![
            PickerItem {
                label: "Default model".into(),
                description: config.model.clone().unwrap_or_else(|| model.to_string()),
                action: PickerAction::OpenModels { save: true },
            },
            edit(
                SettingsItem::MaxOutputTokens,
                "Max output tokens",
                config.max_tokens.to_string(),
            ),
            PickerItem {
                label: "Codex reasoning effort".into(),
                description: config
                    .reasoning_effort
                    .clone()
                    .unwrap_or_else(|| "provider default".into()),
                action: if crate::model::is_codex(config, model) {
                    PickerAction::OpenReasoning { save: true }
                } else {
                    PickerAction::EditSetting {
                        key: "reasoning_effort".into(),
                        initial: config
                            .reasoning_effort
                            .clone()
                            .unwrap_or_else(|| "default".into()),
                        location: location(SettingsItem::ReasoningEffort),
                    }
                },
            },
        ],
        SettingsCategory::Interface => vec![
            PickerItem {
                label: "Reasoning display".into(),
                description: format!(
                    "{} · Enter to toggle",
                    if config.hide_reasoning {
                        "Hidden"
                    } else {
                        "Visible"
                    }
                ),
                action: PickerAction::SetHideReasoning(!config.hide_reasoning),
            },
            PickerItem {
                label: "Accent color".into(),
                description: config.accent_color.config_value(),
                action: PickerAction::OpenAccentColor,
            },
            PickerItem {
                label: "Selection color".into(),
                description: UiColor::selection_config_value(config.selection_color),
                action: PickerAction::OpenSelectionColor,
            },
            PickerItem {
                label: "Status bar".into(),
                description: format!(
                    "{} items · Enter to customize",
                    config.status_bar.items.len()
                ),
                action: PickerAction::OpenStatusBarEditor { selected: 0 },
            },
            PickerItem {
                label: "Scroll bar".into(),
                description: format!(
                    "{} · Enter to toggle",
                    if config.scroll_bar {
                        "Visible"
                    } else {
                        "Hidden"
                    }
                ),
                action: PickerAction::SetScrollBar(!config.scroll_bar),
            },
            PickerItem {
                label: "Auto-hide scroll bar".into(),
                description: format!("{} · Enter to toggle", on_off(config.scroll_bar_auto_hide)),
                action: PickerAction::SetScrollBarAutoHide(!config.scroll_bar_auto_hide),
            },
        ],
        SettingsCategory::Context => vec![
            PickerItem {
                label: "Automatic compaction".into(),
                description: format!("{} · Enter to toggle", on_off(config.auto_compact)),
                action: PickerAction::SetAutoCompact(!config.auto_compact),
            },
            edit(
                SettingsItem::CompactThreshold,
                "Compaction threshold",
                format!("{:.0}%", config.compact_threshold * 100.0),
            ),
            edit(
                SettingsItem::ContextWindow,
                "Current model context window",
                context_window.to_string(),
            ),
        ],
        SettingsCategory::Providers => {
            let mut entries = vec![PickerItem {
                label: "Add or update provider…".into(),
                description: "Guided authentication and model discovery".into(),
                action: PickerAction::OpenConnect {
                    from_settings: true,
                },
            }];
            entries.extend(
                crate::onboarding::provider::provider_catalog(config)
                    .into_iter()
                    .filter(|provider| provider.id != ProviderId::Other)
                    .map(|provider| PickerItem {
                        label: provider.label,
                        description: format!(
                            "{} · {}",
                            if provider.configured {
                                "Configured"
                            } else {
                                "Not configured"
                            },
                            provider.description
                        ),
                        action: PickerAction::OpenConnect {
                            from_settings: true,
                        },
                    }),
            );
            entries
        }
        SettingsCategory::Web => vec![
            PickerItem {
                label: "Web browsing".into(),
                description: format!("{} · Enter to toggle", on_off(config.web_browsing)),
                action: PickerAction::SetWebBrowsing(!config.web_browsing),
            },
            PickerItem {
                label: "Search provider".into(),
                description: format!(
                    "{} · Enter to choose",
                    web_search_provider_label(config.web_search_provider)
                ),
                action: PickerAction::OpenWebSearchProviders,
            },
            edit(
                SettingsItem::WebFetchMaxChars,
                "Maximum fetched characters",
                config.web_fetch_max_chars.to_string(),
            ),
            PickerItem {
                label: "Brave API key".into(),
                description: web_key_status("BRAVE_API_KEY", config.brave_api_key.as_deref()),
                action: PickerAction::EditSecretSetting {
                    key: "brave_api_key".into(),
                    location: location(SettingsItem::BraveApiKey),
                },
            },
            PickerItem {
                label: "Firecrawl API key".into(),
                description: web_key_status(
                    "FIRECRAWL_API_KEY",
                    config.firecrawl_api_key.as_deref(),
                ),
                action: PickerAction::EditSecretSetting {
                    key: "firecrawl_api_key".into(),
                    location: location(SettingsItem::FirecrawlApiKey),
                },
            },
        ],
        SettingsCategory::Subagents => vec![
            PickerItem {
                label: "Parallel subagents".into(),
                description: format!("{} · Enter to toggle", on_off(config.subagents)),
                action: PickerAction::SetSubagents(!config.subagents),
            },
            edit(
                SettingsItem::MaxSubagents,
                "Maximum active subagents",
                config.max_subagents.to_string(),
            ),
            edit(
                SettingsItem::SubagentModel,
                "Default subagent model",
                config.subagent_model.clone(),
            ),
            edit(
                SettingsItem::SubagentRequestBudget,
                "Subagent request budget",
                config.subagent_request_budget.to_string(),
            ),
            edit(
                SettingsItem::SubagentTimeout,
                "Subagent timeout",
                config.subagent_timeout_secs.to_string(),
            ),
        ],
        SettingsCategory::Skills => {
            let mut entries = vec![PickerItem {
                label: "Add directory…".into(),
                description: format!("{} configured", config.skill_dirs.len()),
                action: PickerAction::EditSetting {
                    key: "skills add".into(),
                    initial: String::new(),
                    location: location(SettingsItem::AddSkillDirectory),
                },
            }];
            entries.extend(config.skill_dirs.iter().map(|directory| PickerItem {
                label: format!("Remove {}", directory.display()),
                description: "Stop searching this directory".into(),
                action: PickerAction::ApplySetting {
                    argument: format!("skills remove {}", directory.display()),
                    location: location(SettingsItem::AddSkillDirectory),
                },
            }));
            entries
        }
        SettingsCategory::Advanced => vec![
            PickerItem {
                label: "Reload configuration".into(),
                description: "Read global and project files again".into(),
                action: PickerAction::Reload,
            },
            PickerItem {
                label: "Configuration details".into(),
                description: "Show paths, providers, and command forms".into(),
                action: PickerAction::ShowSettings,
            },
        ],
    };
    let selected = selected.min(items.len().saturating_sub(1));
    Picker {
        title: format!("Settings · {}", category.title()),
        hint: "↑/↓ move  Enter change  Esc back".into(),
        items: std::mem::take(&mut items),
        selected,
        editing: None,
        parent: Some(PickerAction::OpenSettingsRoot {
            selected: category.index(),
        }),
    }
}

fn setting_key(item: SettingsItem) -> &'static str {
    match item {
        SettingsItem::MaxOutputTokens => "max_tokens",
        SettingsItem::CompactThreshold => "compact_threshold",
        SettingsItem::ContextWindow => "context_window",
        SettingsItem::WebSearchProvider => "web_search_provider",
        SettingsItem::WebFetchMaxChars => "web_fetch_max_chars",
        SettingsItem::MaxSubagents => "max_subagents",
        SettingsItem::SubagentModel => "subagent_model",
        SettingsItem::SubagentRequestBudget => "subagent_request_budget",
        SettingsItem::SubagentTimeout => "subagent_timeout_secs",
        _ => "",
    }
}

fn web_key_status(environment: &str, stored: Option<&str>) -> String {
    if std::env::var(environment).is_ok_and(|value| !value.trim().is_empty()) {
        format!("Set by {environment} · Enter to edit config fallback")
    } else if stored.is_some() {
        "Stored in config · Enter to replace or type - to remove".into()
    } else {
        "Not set · Enter to add".into()
    }
}

fn web_search_provider_label(provider: WebSearchProvider) -> &'static str {
    match provider {
        WebSearchProvider::DuckDuckGo => "DuckDuckGo",
        WebSearchProvider::Firecrawl => "Firecrawl",
        WebSearchProvider::Brave => "Brave",
    }
}

pub(super) fn web_search_provider_picker(current: WebSearchProvider) -> Picker {
    let providers = [
        (
            WebSearchProvider::DuckDuckGo,
            "Free search without an API key",
        ),
        (
            WebSearchProvider::Firecrawl,
            "Uses the configured Firecrawl API key",
        ),
        (
            WebSearchProvider::Brave,
            "Uses the configured Brave API key",
        ),
    ];
    let items = providers
        .into_iter()
        .map(|(provider, description)| PickerItem {
            label: web_search_provider_label(provider).into(),
            description: description.into(),
            action: PickerAction::SetWebSearchProvider(provider),
        })
        .collect::<Vec<_>>();
    let selected = providers
        .iter()
        .position(|(provider, _)| *provider == current)
        .unwrap_or(0);
    Picker {
        title: "Search provider".into(),
        hint: "↑/↓ move  Enter select  Esc cancel".into(),
        items,
        selected,
        editing: None,
        parent: Some(PickerAction::OpenSettingsCategory {
            category: SettingsCategory::Web,
            selected: settings_item_index(SettingsCategory::Web, SettingsItem::WebSearchProvider),
        }),
    }
}

pub(super) fn color_picker(current: UiColor) -> Picker {
    let mut items = COLOR_PALETTE
        .into_iter()
        .map(|(label, color)| PickerItem {
            label: label.into(),
            description: swatch_description(color),
            action: PickerAction::SetAccentColor(color),
        })
        .collect::<Vec<_>>();
    items.push(PickerItem {
        label: "Custom RGB…".into(),
        description: "Enter #RRGGBB".into(),
        action: PickerAction::EditSetting {
            key: "accent_color".into(),
            initial: current.config_value(),
            location: Some(SettingsLocation {
                category: SettingsCategory::Interface,
                item: SettingsItem::AccentColor,
            }),
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
        parent: Some(PickerAction::OpenSettingsCategory {
            category: SettingsCategory::Interface,
            selected: settings_item_index(SettingsCategory::Interface, SettingsItem::AccentColor),
        }),
    }
}

pub(super) fn selection_color_picker(current: Option<UiColor>) -> Picker {
    let mut items = vec![PickerItem {
        label: "Accent".into(),
        description: "Follow the accent color".into(),
        action: PickerAction::SetSelectionColor(None),
    }];
    items.extend(COLOR_PALETTE.into_iter().map(|(label, color)| PickerItem {
        label: label.into(),
        description: swatch_description(color),
        action: PickerAction::SetSelectionColor(Some(color)),
    }));
    items.push(PickerItem {
        label: "Custom RGB…".into(),
        description: "Enter accent or #RRGGBB".into(),
        action: PickerAction::EditSetting {
            key: "selection_color".into(),
            initial: UiColor::selection_config_value(current),
            location: Some(SettingsLocation {
                category: SettingsCategory::Interface,
                item: SettingsItem::SelectionColor,
            }),
        },
    });
    let selected = items
        .iter()
        .position(|item| {
            matches!(
                item.action,
                PickerAction::SetSelectionColor(color) if color == current
            )
        })
        .unwrap_or(items.len().saturating_sub(1));
    Picker {
        title: "Selection color".into(),
        hint: "↑/↓ move  Enter select  Esc cancel".into(),
        items,
        selected,
        editing: None,
        parent: Some(PickerAction::OpenSettingsCategory {
            category: SettingsCategory::Interface,
            selected: settings_item_index(
                SettingsCategory::Interface,
                SettingsItem::SelectionColor,
            ),
        }),
    }
}

pub(super) fn status_bar_editor_picker(state: &ViewState, selected: usize) -> Picker {
    let layout = state.status_bar_draft.as_ref().unwrap_or(&state.status_bar);
    let mut items = layout
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let label = match item.label.as_deref() {
                None => "default label".to_string(),
                Some("") => "no label".to_string(),
                Some(label) => format!("label {label:?}"),
            };
            PickerItem {
                label: item.kind.label().into(),
                description: format!(
                    "{} · {} · {label}",
                    item.format.as_str(),
                    item.visibility.as_str()
                ),
                action: PickerAction::OpenStatusBarItem { index, selected: 0 },
            }
        })
        .collect::<Vec<_>>();
    items.extend([
        PickerItem {
            label: "Add item…".into(),
            description: format!(
                "{} available",
                StatusBarKind::ALL.len().saturating_sub(layout.items.len())
            ),
            action: PickerAction::OpenStatusBarAdd,
        },
        PickerItem {
            label: "Color style".into(),
            description: layout.style.as_str().into(),
            action: PickerAction::OpenStatusBarStyles,
        },
        PickerItem {
            label: "Separator".into(),
            description: separator_description(&layout.separator),
            action: PickerAction::OpenStatusBarSeparators,
        },
        PickerItem {
            label: "Reset to default".into(),
            description: "Restore the built-in items and appearance".into(),
            action: PickerAction::ResetStatusBar,
        },
        PickerItem {
            label: "Save".into(),
            description: "Write this layout to global settings".into(),
            action: PickerAction::SaveStatusBar,
        },
    ]);
    Picker {
        title: "Status bar".into(),
        hint: "K/J reorder  d remove  Enter edit  Esc discard".into(),
        selected: selected.min(items.len().saturating_sub(1)),
        items,
        editing: None,
        parent: Some(PickerAction::CancelStatusBarEditor),
    }
}

pub(super) fn status_bar_item_picker(state: &ViewState, index: usize, selected: usize) -> Picker {
    let item = &state
        .status_bar_draft
        .as_ref()
        .unwrap_or(&state.status_bar)
        .items[index];
    let label_description = match item.label.as_deref() {
        None => "Built-in label".into(),
        Some("") => "Hidden".into(),
        Some(label) => label.into(),
    };
    let mut items = vec![
        PickerItem {
            label: "Format".into(),
            description: item.format.as_str().into(),
            action: PickerAction::OpenStatusBarFormats(index),
        },
        PickerItem {
            label: "Custom label…".into(),
            description: label_description,
            action: PickerAction::EditStatusBarLabel {
                index,
                initial: item.label.clone().unwrap_or_default(),
            },
        },
        PickerItem {
            label: "Use built-in label".into(),
            description: "Clear the label override".into(),
            action: PickerAction::ResetStatusBarLabel(index),
        },
    ];
    if item.kind.is_dynamic() {
        items.push(PickerItem {
            label: "Visibility".into(),
            description: item.visibility.as_str().into(),
            action: PickerAction::SetStatusBarVisibility {
                index,
                visibility: if item.visibility == StatusBarVisibility::Auto {
                    StatusBarVisibility::Always
                } else {
                    StatusBarVisibility::Auto
                },
            },
        });
    }
    Picker {
        title: item.kind.label().into(),
        hint: "Enter change  Esc back".into(),
        selected: selected.min(items.len().saturating_sub(1)),
        items,
        editing: None,
        parent: Some(PickerAction::OpenStatusBarEditor { selected: index }),
    }
}

pub(super) fn status_bar_format_picker(state: &ViewState, index: usize) -> Picker {
    let item = &state
        .status_bar_draft
        .as_ref()
        .unwrap_or(&state.status_bar)
        .items[index];
    let items = StatusBarFormat::ALL
        .into_iter()
        .map(|format| PickerItem {
            label: format.as_str().into(),
            description: match format {
                StatusBarFormat::Current => "Match the original status bar",
                StatusBarFormat::Compact => "Show the shortest useful value",
                StatusBarFormat::Detailed => "Add a descriptive label and detail",
            }
            .into(),
            action: PickerAction::SetStatusBarFormat { index, format },
        })
        .collect::<Vec<_>>();
    let selected = StatusBarFormat::ALL
        .iter()
        .position(|format| *format == item.format)
        .unwrap_or(0);
    Picker {
        title: format!("{} format", item.kind.label()),
        hint: "Enter select  Esc back".into(),
        selected,
        items,
        editing: None,
        parent: Some(PickerAction::OpenStatusBarItem { index, selected: 0 }),
    }
}

pub(super) fn status_bar_add_picker(state: &ViewState) -> Picker {
    let layout = state.status_bar_draft.as_ref().unwrap_or(&state.status_bar);
    let items = StatusBarKind::ALL
        .into_iter()
        .filter(|kind| !layout.items.iter().any(|item| item.kind == *kind))
        .map(|kind| PickerItem {
            label: kind.label().into(),
            description: kind.as_str().into(),
            action: PickerAction::AddStatusBarItem(kind),
        })
        .collect::<Vec<_>>();
    Picker {
        title: "Add status item".into(),
        hint: "Enter add  Esc back".into(),
        selected: 0,
        items,
        editing: None,
        parent: Some(PickerAction::OpenStatusBarEditor {
            selected: layout.items.len(),
        }),
    }
}

pub(super) fn status_bar_style_picker(state: &ViewState) -> Picker {
    let layout = state.status_bar_draft.as_ref().unwrap_or(&state.status_bar);
    let items = StatusBarStyle::ALL
        .into_iter()
        .map(|style| PickerItem {
            label: style.as_str().into(),
            description: match style {
                StatusBarStyle::Mixed => "Accent model, mute other items",
                StatusBarStyle::Accent => "Use the accent color for every item",
                StatusBarStyle::Muted => "Use the muted accent for every item",
                StatusBarStyle::Plain => "Use the terminal's default foreground",
            }
            .into(),
            action: PickerAction::SetStatusBarStyle(style),
        })
        .collect::<Vec<_>>();
    let selected = StatusBarStyle::ALL
        .iter()
        .position(|style| *style == layout.style)
        .unwrap_or(0);
    Picker {
        title: "Status-bar color style".into(),
        hint: "Enter select  Esc back".into(),
        selected,
        items,
        editing: None,
        parent: Some(PickerAction::OpenStatusBarEditor {
            selected: layout.items.len() + 1,
        }),
    }
}

pub(super) fn status_bar_separator_picker(state: &ViewState) -> Picker {
    let layout = state.status_bar_draft.as_ref().unwrap_or(&state.status_bar);
    let presets = [
        ("Dot", "  ·  "),
        ("Pipe", " | "),
        ("Slash", " / "),
        ("Space", " "),
        ("None", ""),
    ];
    let mut items = presets
        .into_iter()
        .map(|(label, separator)| PickerItem {
            label: label.into(),
            description: separator_description(separator),
            action: PickerAction::SetStatusBarSeparator(separator.into()),
        })
        .collect::<Vec<_>>();
    items.push(PickerItem {
        label: "Custom…".into(),
        description: separator_description(&layout.separator),
        action: PickerAction::EditStatusBarSeparator(layout.separator.clone()),
    });
    let selected = items
        .iter()
        .position(|item| {
            matches!(&item.action, PickerAction::SetStatusBarSeparator(separator) if separator == &layout.separator)
        })
        .unwrap_or(items.len() - 1);
    Picker {
        title: "Status-bar separator".into(),
        hint: "Enter select  Esc back".into(),
        selected,
        items,
        editing: None,
        parent: Some(PickerAction::OpenStatusBarEditor {
            selected: layout.items.len() + 2,
        }),
    }
}

fn separator_description(separator: &str) -> String {
    if separator.is_empty() {
        "No separator".into()
    } else if separator.chars().all(char::is_whitespace) {
        format!(
            "{} space columns",
            unicode_width::UnicodeWidthStr::width(separator)
        )
    } else {
        format!("{separator:?}")
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

pub(super) fn settings_item_index(category: SettingsCategory, item: SettingsItem) -> usize {
    settings_items(category)
        .iter()
        .position(|candidate| *candidate == item)
        .unwrap_or(0)
}

fn settings_items(category: SettingsCategory) -> &'static [SettingsItem] {
    match category {
        SettingsCategory::Model => &[
            SettingsItem::DefaultModel,
            SettingsItem::MaxOutputTokens,
            SettingsItem::ReasoningEffort,
        ],
        SettingsCategory::Interface => &[
            SettingsItem::ReasoningDisplay,
            SettingsItem::AccentColor,
            SettingsItem::SelectionColor,
            SettingsItem::StatusBar,
            SettingsItem::ScrollBar,
            SettingsItem::ScrollBarAutoHide,
        ],
        SettingsCategory::Context => &[
            SettingsItem::AutoCompact,
            SettingsItem::CompactThreshold,
            SettingsItem::ContextWindow,
        ],
        SettingsCategory::Providers => &[SettingsItem::ProviderSetup],
        SettingsCategory::Web => &[
            SettingsItem::WebBrowsingEnabled,
            SettingsItem::WebSearchProvider,
            SettingsItem::WebFetchMaxChars,
            SettingsItem::BraveApiKey,
            SettingsItem::FirecrawlApiKey,
        ],
        SettingsCategory::Subagents => &[
            SettingsItem::SubagentsEnabled,
            SettingsItem::MaxSubagents,
            SettingsItem::SubagentModel,
            SettingsItem::SubagentRequestBudget,
            SettingsItem::SubagentTimeout,
        ],
        SettingsCategory::Skills => &[SettingsItem::AddSkillDirectory],
        SettingsCategory::Advanced => &[SettingsItem::Reload, SettingsItem::ConfigurationDetails],
    }
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
