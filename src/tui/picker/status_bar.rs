//! Status-bar editor picker construction.
//!
//! The picker facade owns the shared model and keyboard reducer; this child
//! builds the status-bar customization menus from view state.

use crate::config::{StatusBarFormat, StatusBarKind, StatusBarStyle, StatusBarVisibility};

use super::super::ViewState;
use super::{Picker, PickerAction, PickerItem};

pub(in crate::tui) fn status_bar_editor_picker(state: &ViewState, selected: usize) -> Picker {
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

pub(in crate::tui) fn status_bar_item_picker(
    state: &ViewState,
    index: usize,
    selected: usize,
) -> Picker {
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

pub(in crate::tui) fn status_bar_format_picker(state: &ViewState, index: usize) -> Picker {
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

pub(in crate::tui) fn status_bar_add_picker(state: &ViewState) -> Picker {
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

pub(in crate::tui) fn status_bar_style_picker(state: &ViewState) -> Picker {
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

pub(in crate::tui) fn status_bar_separator_picker(state: &ViewState) -> Picker {
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
