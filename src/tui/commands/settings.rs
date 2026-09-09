//! Settings command parsing and application.
//!
//! The commands facade owns slash-command dispatch; this child parses
//! `/settings` and `/reasoning` arguments, applies configuration changes,
//! and renders settings and usage summaries.

use crate::agent::Agent;
use crate::config::{
    Config, ConfigChange, ConfigChangeEffect, MAX_SUBAGENT_REQUEST_BUDGET,
    MAX_SUBAGENT_TIMEOUT_SECS, MAX_WEB_FETCH_MAX_CHARS, SkillDirectoryAction, UiColor,
    parse_bounded, parse_builtin_api_key, parse_on_off, parse_threshold,
};
use crate::error::Error;

use super::super::ViewState;
use super::super::picker::{
    SettingsCategory, SettingsItem, SettingsLocation, settings_category_picker, settings_item_index,
};
use super::super::render::format_token_count;

pub(in crate::tui) fn reasoning(agent: &mut Agent, argument: &str, state: &mut ViewState) {
    let supported = crate::model::reasoning_efforts(agent.config(), agent.model());
    if argument.is_empty() {
        if supported.is_empty() {
            state.notice(format!(
                "{} does not expose reasoning levels.",
                agent.model()
            ));
        } else {
            super::super::picker::open_reasoning_picker(agent, state, false);
        }
        return;
    }
    let effort = argument.to_ascii_lowercase();
    if effort == "default" {
        apply_reasoning_effort(agent, None, state);
    } else if supported.iter().any(|level| *level == effort) {
        apply_reasoning_effort(agent, Some(effort), state);
    } else if supported.is_empty() {
        state.notice(format!(
            "{} does not expose reasoning levels.",
            agent.model()
        ));
    } else {
        state.notice(format!(
            "{} supports default, {}.",
            agent.model(),
            supported.join(", ")
        ));
    }
}

pub(super) fn apply_reasoning_effort(
    agent: &mut Agent,
    effort: Option<String>,
    state: &mut ViewState,
) {
    agent.set_reasoning_effort(effort.clone());
    state.reasoning_effort = effort;
    let label = state
        .reasoning_effort
        .as_deref()
        .unwrap_or("provider default");
    state.notice(format!("Using {} with {label} reasoning.", agent.model()));
}

pub(in crate::tui) fn refresh_model_selection(agent: &Agent, state: &mut ViewState) {
    state.model = agent.model().to_string();
    state.reasoning_effort =
        crate::model::effective_reasoning_effort(agent.config(), agent.model()).map(str::to_string);
    state.context_window = agent.context_window();
    state.context_tokens = 0;
}

pub(super) fn interface_location(item: SettingsItem) -> SettingsLocation {
    SettingsLocation {
        category: SettingsCategory::Interface,
        item,
    }
}

pub(super) fn open_settings_location(
    agent: &Agent,
    state: &mut ViewState,
    location: SettingsLocation,
) {
    state.picker = Some(settings_category_picker(
        agent,
        location.category,
        settings_item_index(location.category, location.item),
    ));
}

pub(in crate::tui) fn settings(agent: &mut Agent, argument: &str, state: &mut ViewState) -> bool {
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
        "bell" => one_value(&mut parts, "usage: /settings bell on|off")
            .and_then(parse_on_off)
            .map(ConfigChange::Bell),
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

pub(in crate::tui) fn apply_config_change(
    agent: &mut Agent,
    change: ConfigChange,
    state: &mut ViewState,
) -> bool {
    match agent.change_global_config(change) {
        Ok(effect) => {
            state.model = agent.model().to_string();
            state.reasoning_effort =
                crate::model::effective_reasoning_effort(agent.config(), agent.model())
                    .map(str::to_string);
            state.hide_reasoning = agent.config().hide_reasoning;
            state.accent_color = agent.config().accent_color;
            state.selection_color = agent.config().effective_selection_color();
            state.status_bar = agent.config().status_bar.clone();
            state.subagents_enabled = agent.config().subagents;
            state.sync_scroll_bar_config(agent.config());
            state.bell = agent.config().bell;
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

pub(in crate::tui) fn notice_config_effect(
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

pub(in crate::tui) fn show_settings(agent: &Agent, state: &mut ViewState) {
    let mut providers = agent.config().providers.iter().collect::<Vec<_>>();
    providers.sort_by_key(|(name, _)| name.as_str());
    let mut text = format!(
        "Settings\n\n- model: `{}`\n- max_tokens: `{}`\n- reasoning_effort: `{}`\n- hide_reasoning: `{}`\n- accent_color: `{}`\n- selection_color: `{}`\n- status_bar: `{} items, {} style`\n- scroll_bar: `{}`\n- scroll_bar_auto_hide: `{}`\n- bell: `{}`\n- auto_compact: `{}`\n- compact_threshold: `{:.0}%`\n- context_window for current model: `{}`\n- web_browsing: `{}`\n- web_search_provider: `{}`\n- web_fetch_max_chars: `{}`\n- brave_api_key: `{}`\n- firecrawl_api_key: `{}`\n- subagents: `{}`\n- max_subagents: `{}`\n- subagent_model: `{}`\n- subagent_request_budget: `{}`\n- subagent_timeout_secs: `{}`\n- anthropic_base_url: `{}`\n- openai_base_url: `{}`\n- anthropic_api_key: `{}`\n- openai_api_key: `{}`\n\nSkill directories\n\n",
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
        if agent.config().bell { "on" } else { "off" },
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
        "\nChanges are written to `{}`. Project settings in `./.yawl/config.json` override them.\n\nCommands\n\n- `/settings model MODEL`\n- `/settings max_tokens NUMBER`\n- `/settings reasoning_effort default|minimal|low|medium|high|xhigh|max`\n- `/settings hide_reasoning on|off`\n- `/settings accent_color NAME|#RRGGBB`\n- `/settings selection_color accent|NAME|#RRGGBB`\n- `/settings scroll_bar on|off`\n- `/settings scroll_bar_auto_hide on|off`\n- `/settings bell on|off`\n- `/settings auto_compact on|off`\n- `/settings compact_threshold 85%`\n- `/settings context_window TOKENS`\n- `/settings web_browsing on|off`\n- `/settings web_search_provider duckduckgo|brave|firecrawl`\n- `/settings web_fetch_max_chars NUMBER`\n- `/settings brave_api_key KEY|-`\n- `/settings firecrawl_api_key KEY|-`\n- `/settings subagents on|off`\n- `/settings max_subagents NUMBER`\n- `/settings subagent_model inherit|MODEL`\n- `/settings subagent_request_budget NUMBER|0`\n- `/settings subagent_timeout_secs SECONDS|0`\n- `/settings skills add|remove DIRECTORY`\n- `/settings provider NAME BASE_URL [API_KEY|-]`\n- `/settings openai_base_url URL`\n- `/settings anthropic_base_url URL`\n- `/settings anthropic_api_key KEY|-`\n- `/settings openai_api_key KEY|-`\n- `/settings reload`\n\nUse an environment reference such as `$OMLX_API_KEY` instead of putting a secret directly in terminal history. Pass `-` as a key value to remove a saved key.",
        agent.config().global_config_path().display()
    ));
    state.notice(text);
}

pub(in crate::tui) fn show_usage(state: &mut ViewState) {
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
        format_token_count(tokens.input_tokens),
        format_token_count(tokens.fresh_input_tokens()),
        format_token_count(tokens.cached_input_tokens),
    )
    .expect("writing to a String cannot fail");
    writeln!(
        text,
        "- Cache writes: {}",
        format_token_count(tokens.cache_write_input_tokens),
    )
    .expect("writing to a String cannot fail");
    writeln!(
        text,
        "- Output: {}",
        format_token_count(tokens.output_tokens),
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
