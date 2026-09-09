//! Settings, color, and search-provider picker construction.
//!
//! The picker facade owns the shared model and keyboard reducer; this child
//! builds the settings catalogs from configuration.

use crate::agent::Agent;
use crate::config::{Config, UiColor, WebSearchProvider};
use crate::onboarding::provider::ProviderId;

use super::super::ViewState;
use super::{Picker, PickerAction, PickerItem, SettingsCategory, SettingsItem, SettingsLocation};

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

pub(in crate::tui) fn open_settings_picker(agent: &Agent, state: &mut ViewState) {
    state.picker = Some(settings_picker(agent));
}

pub(in crate::tui) fn settings_picker(_agent: &Agent) -> Picker {
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

pub(in crate::tui) fn settings_category_picker(
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

pub(in crate::tui) fn settings_category_picker_from(
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
            PickerItem {
                label: "Bell".into(),
                description: format!("{} · Enter to toggle", on_off(config.bell)),
                action: PickerAction::SetBell(!config.bell),
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

pub(in crate::tui) fn web_search_provider_picker(current: WebSearchProvider) -> Picker {
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

pub(in crate::tui) fn color_picker(current: UiColor) -> Picker {
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

pub(in crate::tui) fn selection_color_picker(current: Option<UiColor>) -> Picker {
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

pub(in crate::tui) fn settings_item_index(category: SettingsCategory, item: SettingsItem) -> usize {
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
            SettingsItem::Bell,
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
