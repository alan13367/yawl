//! Configurable status-bar editor, item formatting, and row composition.

use crate::config::{
    StatusBarConfig, StatusBarFormat, StatusBarItemConfig, StatusBarKind, StatusBarStyle,
    StatusBarVisibility,
};

use super::ViewState;
use super::markdown;
use super::picker::{
    PickerAction, status_bar_add_picker, status_bar_editor_picker, status_bar_format_picker,
    status_bar_item_picker, status_bar_separator_picker, status_bar_style_picker,
};
use super::render::{foreground_color, status_style};

#[derive(Clone, Copy)]
enum LabelPosition {
    Before,
    After,
}

pub(super) fn active_config(state: &ViewState) -> &StatusBarConfig {
    state.status_bar_draft.as_ref().unwrap_or(&state.status_bar)
}

/// Applies status-bar editor actions that behave the same whether a turn is
/// idle or active. Save and cancel are returned to the caller because they
/// need access to different config owners in those two states.
pub(super) fn handle_editor_action(
    state: &mut ViewState,
    action: PickerAction,
) -> Option<PickerAction> {
    match action {
        PickerAction::OpenStatusBarEditor { selected } => {
            if state.status_bar_draft.is_none() {
                state.status_bar_draft = Some(state.status_bar.clone());
            }
            state.picker = Some(status_bar_editor_picker(state, selected));
        }
        PickerAction::OpenStatusBarItem { index, selected } => {
            state.picker = Some(status_bar_item_picker(state, index, selected));
        }
        PickerAction::OpenStatusBarFormats(index) => {
            state.picker = Some(status_bar_format_picker(state, index));
        }
        PickerAction::SetStatusBarFormat { index, format } => {
            if let Some(item) = draft_item_mut(state, index) {
                item.format = format;
            }
            state.picker = Some(status_bar_item_picker(state, index, 0));
        }
        PickerAction::SetStatusBarVisibility { index, visibility } => {
            if let Some(item) = draft_item_mut(state, index) {
                item.visibility = visibility;
            }
            state.picker = Some(status_bar_item_picker(state, index, 3));
        }
        PickerAction::ApplyStatusBarLabel { index, label } => {
            let previous = draft_item_mut(state, index).map(|item| item.label.replace(label));
            if let Err(error) = validate_draft(state) {
                if let (Some(item), Some(previous)) = (draft_item_mut(state, index), previous) {
                    item.label = previous;
                }
                state.notice(format!("Could not change status bar: {error}"));
            }
            state.picker = Some(status_bar_item_picker(state, index, 1));
        }
        PickerAction::ResetStatusBarLabel(index) => {
            if let Some(item) = draft_item_mut(state, index) {
                item.label = None;
            }
            state.picker = Some(status_bar_item_picker(state, index, 2));
        }
        PickerAction::OpenStatusBarAdd => {
            state.picker = Some(status_bar_add_picker(state));
        }
        PickerAction::AddStatusBarItem(kind) => {
            if let Some(layout) = state.status_bar_draft.as_mut()
                && !layout.items.iter().any(|item| item.kind == kind)
            {
                layout.items.push(StatusBarItemConfig::new(kind));
            }
            let selected = state
                .status_bar_draft
                .as_ref()
                .map_or(0, |layout| layout.items.len().saturating_sub(1));
            state.picker = Some(status_bar_editor_picker(state, selected));
        }
        PickerAction::MoveStatusBarItem { index, direction } => {
            let mut selected = index;
            if let Some(layout) = state.status_bar_draft.as_mut() {
                let target = index.saturating_add_signed(direction);
                if index < layout.items.len() && target < layout.items.len() {
                    layout.items.swap(index, target);
                    selected = target;
                }
            }
            state.picker = Some(status_bar_editor_picker(state, selected));
        }
        PickerAction::RemoveStatusBarItem(index) => {
            if let Some(layout) = state.status_bar_draft.as_mut()
                && index < layout.items.len()
            {
                layout.items.remove(index);
            }
            let selected = state
                .status_bar_draft
                .as_ref()
                .map_or(0, |layout| index.min(layout.items.len().saturating_sub(1)));
            state.picker = Some(status_bar_editor_picker(state, selected));
        }
        PickerAction::OpenStatusBarStyles => {
            state.picker = Some(status_bar_style_picker(state));
        }
        PickerAction::SetStatusBarStyle(style) => {
            if let Some(layout) = state.status_bar_draft.as_mut() {
                layout.style = style;
            }
            let selected = state
                .status_bar_draft
                .as_ref()
                .map_or(1, |layout| layout.items.len() + 1);
            state.picker = Some(status_bar_editor_picker(state, selected));
        }
        PickerAction::OpenStatusBarSeparators => {
            state.picker = Some(status_bar_separator_picker(state));
        }
        PickerAction::SetStatusBarSeparator(separator)
        | PickerAction::ApplyStatusBarSeparator(separator) => {
            let previous = state
                .status_bar_draft
                .as_mut()
                .map(|layout| std::mem::replace(&mut layout.separator, separator));
            if let Err(error) = validate_draft(state) {
                if let (Some(layout), Some(previous)) = (state.status_bar_draft.as_mut(), previous)
                {
                    layout.separator = previous;
                }
                state.notice(format!("Could not change status bar: {error}"));
            }
            let selected = state
                .status_bar_draft
                .as_ref()
                .map_or(2, |layout| layout.items.len() + 2);
            state.picker = Some(status_bar_editor_picker(state, selected));
        }
        PickerAction::ResetStatusBar => {
            state.status_bar_draft = Some(StatusBarConfig::default());
            state.picker = Some(status_bar_editor_picker(state, 0));
        }
        PickerAction::EditStatusBarLabel { .. } | PickerAction::EditStatusBarSeparator(_) => {}
        PickerAction::CancelStatusBarEditor | PickerAction::SaveStatusBar => return Some(action),
        action => return Some(action),
    }
    None
}

fn draft_item_mut(state: &mut ViewState, index: usize) -> Option<&mut StatusBarItemConfig> {
    state
        .status_bar_draft
        .as_mut()
        .and_then(|layout| layout.items.get_mut(index))
}

fn validate_draft(state: &ViewState) -> Result<(), String> {
    state
        .status_bar_draft
        .as_ref()
        .ok_or_else(|| "status-bar editor has no draft".to_string())?
        .validate()
}

pub(super) fn render(state: &ViewState, width: usize) -> Option<String> {
    let config = active_config(state);
    let segments = config
        .items
        .iter()
        .filter_map(|item| render_item(state, item).map(|text| (item.kind, text)))
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return None;
    }

    let separator = styled_separator(config, state);
    let body = segments
        .into_iter()
        .map(|(kind, text)| styled_item(config.style, kind, &text, state))
        .collect::<Vec<_>>()
        .join(&separator);
    Some(markdown::fit_width(&format!(" {body}\x1b[0m"), width))
}

fn styled_item(
    style: StatusBarStyle,
    kind: StatusBarKind,
    text: &str,
    state: &ViewState,
) -> String {
    match style {
        StatusBarStyle::Mixed if kind == StatusBarKind::Model => format!(
            "{}\x1b[1m{text}\x1b[22m\x1b[0m",
            foreground_color(state.accent_color)
        ),
        StatusBarStyle::Mixed | StatusBarStyle::Muted => {
            format!("{}{text}\x1b[0m", status_style(state.accent_color))
        }
        StatusBarStyle::Accent => {
            format!("{}{text}\x1b[0m", foreground_color(state.accent_color))
        }
        StatusBarStyle::Plain => format!("{text}\x1b[0m"),
    }
}

fn styled_separator(config: &StatusBarConfig, state: &ViewState) -> String {
    match config.style {
        StatusBarStyle::Plain => config.separator.clone(),
        StatusBarStyle::Accent => format!(
            "{}{}\x1b[0m",
            foreground_color(state.accent_color),
            config.separator
        ),
        StatusBarStyle::Mixed | StatusBarStyle::Muted => format!(
            "{}{}\x1b[0m",
            status_style(state.accent_color),
            config.separator
        ),
    }
}

fn render_item(state: &ViewState, item: &StatusBarItemConfig) -> Option<String> {
    match item.kind {
        StatusBarKind::Model => Some(render_model(state, item)),
        StatusBarKind::Reasoning => render_reasoning(state, item),
        StatusBarKind::Context => Some(render_context(state, item)),
        StatusBarKind::Cache => render_cache(state, item),
        StatusBarKind::Elapsed => render_elapsed(state, item),
        StatusBarKind::Queued => render_count(
            item,
            state.queued_inputs.len(),
            "queued",
            "queue",
            LabelPosition::After,
        ),
        StatusBarKind::Steering => render_count(
            item,
            state.pending_steers.len(),
            "steering",
            "steering",
            LabelPosition::After,
        ),
        StatusBarKind::Goal => render_goal(state, item),
        StatusBarKind::Pending => render_count(
            item,
            state.pending_actions.len(),
            "pending",
            "settings pending",
            LabelPosition::After,
        ),
        StatusBarKind::ActiveSubagents => render_active_subagents(state, item),
        StatusBarKind::FailedSubagents => render_count(
            item,
            failed_subagent_count(state),
            "failed",
            "agents failed",
            LabelPosition::After,
        ),
        StatusBarKind::ChildTokens => render_child_tokens(state, item),
    }
}

fn render_model(state: &ViewState, item: &StatusBarItemConfig) -> String {
    let core = match item.format {
        StatusBarFormat::Compact => state
            .model
            .split_once(':')
            .map_or(state.model.as_str(), |(_, model)| model),
        StatusBarFormat::Current | StatusBarFormat::Detailed => &state.model,
    };
    let builtin = (item.format == StatusBarFormat::Detailed).then_some("model");
    labeled(item, builtin, LabelPosition::Before, core)
}

fn render_reasoning(state: &ViewState, item: &StatusBarItemConfig) -> Option<String> {
    let effort = state
        .reasoning_effort
        .as_deref()
        .filter(|value| !value.is_empty());
    if effort.is_none() && item.visibility == StatusBarVisibility::Auto {
        return None;
    }
    let core = effort.unwrap_or("provider default");
    let builtin = (item.format == StatusBarFormat::Detailed).then_some("reasoning");
    Some(labeled(item, builtin, LabelPosition::Before, core))
}

fn render_context(state: &ViewState, item: &StatusBarItemConfig) -> String {
    let percentage = state
        .context_tokens
        .saturating_mul(100)
        .checked_div(state.context_window)
        .unwrap_or(0);
    let core = match item.format {
        StatusBarFormat::Compact => format!("{percentage}%"),
        StatusBarFormat::Current => format!(
            "{percentage}% / {}",
            format_token_count(state.context_window)
        ),
        StatusBarFormat::Detailed => format!(
            "{} / {} ({percentage}%)",
            format_token_count(state.context_tokens),
            format_token_count(state.context_window)
        ),
    };
    let builtin = (item.format == StatusBarFormat::Detailed).then_some("context");
    labeled(item, builtin, LabelPosition::Before, &core)
}

fn render_cache(state: &ViewState, item: &StatusBarItemConfig) -> Option<String> {
    if !state.usage.tokens.cache_details_reported && item.visibility == StatusBarVisibility::Auto {
        return None;
    }
    let core = if state.usage.tokens.cache_details_reported {
        format!("{}%", state.usage.cache_hit_percent())
    } else {
        "n/a".into()
    };
    let builtin = match item.format {
        StatusBarFormat::Compact => None,
        StatusBarFormat::Current => Some("cache"),
        StatusBarFormat::Detailed => Some("cache hit"),
    };
    Some(labeled(item, builtin, LabelPosition::Before, &core))
}

fn render_elapsed(state: &ViewState, item: &StatusBarItemConfig) -> Option<String> {
    let elapsed = state.turn_started.map(|started| started.elapsed());
    if elapsed.is_none() && item.visibility == StatusBarVisibility::Auto {
        return None;
    }
    let mut core = elapsed.map_or_else(|| "idle".into(), super::tool_view::format_elapsed);
    if item.format == StatusBarFormat::Compact {
        core.retain(|character| character != ' ');
    }
    let builtin = (item.format == StatusBarFormat::Detailed).then_some("elapsed");
    Some(labeled(item, builtin, LabelPosition::Before, &core))
}

fn render_count(
    item: &StatusBarItemConfig,
    count: usize,
    current_label: &str,
    detailed_label: &str,
    current_position: LabelPosition,
) -> Option<String> {
    if count == 0 && item.visibility == StatusBarVisibility::Auto {
        return None;
    }
    let core = count.to_string();
    let (builtin, position) = match item.format {
        StatusBarFormat::Compact => (None, current_position),
        StatusBarFormat::Current => (Some(current_label), current_position),
        StatusBarFormat::Detailed => (Some(detailed_label), LabelPosition::Before),
    };
    Some(labeled(item, builtin, position, &core))
}

fn render_goal(state: &ViewState, item: &StatusBarItemConfig) -> Option<String> {
    let Some(goal) = state.active_goal.as_deref() else {
        return (item.visibility == StatusBarVisibility::Always).then(|| {
            match item.label.as_deref() {
                None => "no goal".into(),
                Some("") => "none".into(),
                Some(label) => format!("no {label}"),
            }
        });
    };
    let running = state.goal_running;
    if item.format == StatusBarFormat::Compact {
        let state_label = if running { "running" } else { "paused" };
        return Some(match item.label.as_deref() {
            None | Some("") => state_label.into(),
            Some(label) => format!("{label} {state_label}"),
        });
    }
    let preview_limit = if item.format == StatusBarFormat::Detailed {
        64
    } else {
        32
    };
    let preview = crate::error::truncate(goal, preview_limit);
    let state_label = if running { "running" } else { "paused" };
    let default_label = match (item.format, running) {
        (StatusBarFormat::Current, true) => "goal".into(),
        (StatusBarFormat::Current, false) => "goal paused".into(),
        (StatusBarFormat::Detailed, _) => format!("goal {state_label}"),
        (StatusBarFormat::Compact, _) => unreachable!("compact returned above"),
    };
    let label = match item.label.as_deref() {
        None => default_label,
        Some("") if item.format == StatusBarFormat::Detailed => state_label.into(),
        Some("") if running => String::new(),
        Some("") => "paused".into(),
        Some(label) if item.format == StatusBarFormat::Detailed => {
            format!("{label} {state_label}")
        }
        Some(label) if running => label.into(),
        Some(label) => format!("{label} paused"),
    };
    Some(if label.is_empty() {
        preview
    } else {
        format!("{label}: {preview}")
    })
}

fn render_active_subagents(state: &ViewState, item: &StatusBarItemConfig) -> Option<String> {
    let running = state
        .subagent_snapshots
        .iter()
        .filter(|snapshot| snapshot.status.is_active())
        .collect::<Vec<_>>();
    if running.is_empty() && item.visibility == StatusBarVisibility::Auto {
        return None;
    }
    let core = match (item.format, running.as_slice()) {
        (_, []) => "0".into(),
        (StatusBarFormat::Compact | StatusBarFormat::Current, [snapshot]) => snapshot.name.clone(),
        (StatusBarFormat::Compact | StatusBarFormat::Current, many) => many.len().to_string(),
        (StatusBarFormat::Detailed, [snapshot]) => format!("{} running", snapshot.name),
        (StatusBarFormat::Detailed, many) => format!("{} running", many.len()),
    };
    let (builtin, position) = match item.format {
        StatusBarFormat::Compact => (None, LabelPosition::After),
        StatusBarFormat::Current if running.len() == 1 => (None, LabelPosition::After),
        StatusBarFormat::Current => (Some("agents"), LabelPosition::After),
        StatusBarFormat::Detailed if running.len() == 1 => (Some("agent"), LabelPosition::Before),
        StatusBarFormat::Detailed => (Some("agents"), LabelPosition::After),
    };
    Some(labeled(item, builtin, position, &core))
}

fn failed_subagent_count(state: &ViewState) -> usize {
    state
        .subagent_snapshots
        .iter()
        .filter(|snapshot| snapshot.status == crate::subagent::SubagentStatus::Failed)
        .count()
}

fn render_child_tokens(state: &ViewState, item: &StatusBarItemConfig) -> Option<String> {
    if state.subagent_tokens == 0 && item.visibility == StatusBarVisibility::Auto {
        return None;
    }
    let core = format_token_count(state.subagent_tokens);
    let (builtin, position) = match item.format {
        StatusBarFormat::Compact => (None, LabelPosition::After),
        StatusBarFormat::Current => (Some("child"), LabelPosition::After),
        StatusBarFormat::Detailed => (Some("child tokens"), LabelPosition::Before),
    };
    Some(labeled(item, builtin, position, &core))
}

fn labeled(
    item: &StatusBarItemConfig,
    builtin: Option<&str>,
    position: LabelPosition,
    core: &str,
) -> String {
    let label = item.label.as_deref().or(builtin);
    let Some(label) = label.filter(|label| !label.is_empty()) else {
        return core.to_string();
    };
    match position {
        LabelPosition::Before => format!("{label} {core}"),
        LabelPosition::After => format!("{core} {label}"),
    }
}

/// Compact token counts for narrow status lines.
pub(super) fn format_token_count(tokens: u64) -> String {
    if tokens < 10_000 {
        return tokens.to_string();
    }
    if tokens < 1_000_000 {
        let whole = tokens / 1_000;
        let tenths = tokens % 1_000 / 100;
        if tenths == 0 {
            return format!("{whole}k");
        }
        return format!("{whole}.{tenths}k");
    }
    let whole = tokens / 1_000_000;
    let tenths = tokens % 1_000_000 / 100_000;
    if tenths == 0 {
        return format!("{whole}M");
    }
    format!("{whole}.{tenths}M")
}
