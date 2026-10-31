//! Picker model, catalogs, and keyboard reducer.
//!
//! This facade owns the shared picker model and keyboard reducer. Private
//! children build the settings, status-bar editor, model, and reasoning
//! catalogs, edit single-line fields, and render pickers. TUI callers keep
//! the entry points here.

mod field;
mod models;
mod render;
mod settings;
mod status_bar;

pub(super) use field::{FieldTarget, PickerField, field_picker};
pub(super) use models::{
    ModelRefreshJob, open_model_picker, open_model_picker_from_config, open_reasoning_picker,
    open_subagent_model_picker, poll_model_picker, reasoning_picker, reasoning_picker_from_config,
};
#[cfg(test)]
pub(super) use models::{
    model_picker, refreshed_model_picker, refreshed_subagent_model_picker, subagent_model_picker,
};
pub(super) use render::render_picker;

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
    Config, StatusBarFormat, StatusBarKind, StatusBarStyle, StatusBarVisibility, Theme, UiColor,
    WebSearchProvider,
};

use super::ViewState;
use super::connection::{ConnectEditField, ConnectStep};
use super::events::Key;
use super::input::{Editor, Submission};
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
    Theme,
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
    OpenSubagentModels,
    OpenModels {
        save: bool,
    },
    /// Reopen the model picker, restoring `selected` after a canceled removal.
    ReturnToModels {
        save: bool,
        selected: usize,
    },
    ConfirmRemoveModel {
        model: String,
        save: bool,
        selected: usize,
    },
    RemoveModel {
        model: String,
        save: bool,
        selected: usize,
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
    SetTheme(Theme),
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
    /// Codex with a saved login: reuse it, or start a `fresh` device login.
    ConnectLogin {
        fresh: bool,
    },
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
    ConnectToggleReasoning(usize),
    ConnectConfirmReasoning,
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
        input: Submission,
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
    /// Picks the row at this index, for pickers whose caller only needs to
    /// know which row was chosen.
    Choice(usize),
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
    /// The rows shown: every row, or the rows matching the search query.
    pub(super) items: Vec<PickerItem>,
    pub(super) selected: usize,
    pub(super) editing: Option<PickerEdit>,
    /// Action performed by Escape. Standalone pickers leave this unset.
    pub(super) parent: Option<PickerAction>,
    /// Live search over a long list, enabled with [`Picker::searchable`].
    pub(super) filter: Option<PickerFilter>,
}

/// Type-to-search state: the query, every row, and how many trailing rows
/// (such as "Enter manually…") stay visible whatever the query.
#[derive(Clone)]
pub(super) struct PickerFilter {
    pub(super) query: String,
    all: Vec<PickerItem>,
    pinned_tail: usize,
}

impl PickerFilter {
    /// How many rows match the query, and how many there are to search.
    pub(super) fn counts(&self, shown: usize) -> (usize, usize) {
        (
            shown.saturating_sub(self.pinned_tail),
            self.all.len().saturating_sub(self.pinned_tail),
        )
    }
}

impl Picker {
    /// Makes the picker searchable. Printable keys then filter the rows
    /// instead of acting as shortcuts, Backspace edits the query, and Esc
    /// clears it before it closes the picker. The last `pinned_tail` rows
    /// always stay listed.
    pub(super) fn searchable(mut self, pinned_tail: usize) -> Self {
        self.filter = Some(PickerFilter {
            query: String::new(),
            all: self.items.clone(),
            pinned_tail: pinned_tail.min(self.items.len()),
        });
        self
    }

    /// Replaces the search query and shows the rows it matches: every
    /// whitespace-separated term must appear in the label or description,
    /// ignoring case.
    pub(super) fn set_filter(&mut self, query: &str) {
        let Some(filter) = self.filter.as_mut() else {
            return;
        };
        filter.query = query.to_string();
        let terms = query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        let searchable = filter.all.len() - filter.pinned_tail;
        self.items = filter
            .all
            .iter()
            .enumerate()
            .filter(|(index, item)| {
                *index >= searchable || {
                    let haystack = format!("{} {}", item.label, item.description).to_lowercase();
                    terms.iter().all(|term| haystack.contains(term.as_str()))
                }
            })
            .map(|(_, item)| item.clone())
            .collect();
        self.selected = 0;
    }

    /// Applies a key to the search query. Returns whether it was one.
    fn edit_filter(&mut self, key: Key) -> bool {
        let Some(filter) = self.filter.as_ref() else {
            return false;
        };
        match key {
            Key::Char(character) if !character.is_control() => {
                let query = format!("{}{character}", filter.query);
                self.set_filter(&query);
            }
            Key::Backspace if !filter.query.is_empty() => {
                let mut query = filter.query.clone();
                query.pop();
                self.set_filter(&query);
            }
            Key::Escape | Key::Ctrl('c') if !filter.query.is_empty() => self.set_filter(""),
            _ => return false,
        }
        true
    }
}

#[derive(Clone)]
pub(super) enum PickerEdit {
    /// A queued message, edited in the composer so multi-line text and
    /// images keep working.
    Queued(usize),
    /// A single-line value typed into the picker's own field.
    Field(Box<PickerField>),
}

pub(super) struct ActivePickers {
    pub(super) settings: Picker,
    pub(super) settings_categories: Vec<(SettingsCategory, Picker)>,
    pub(super) default_reasoning: Picker,
    pub(super) accent_color: Picker,
    pub(super) selection_color: Picker,
}

impl ActivePickers {
    pub(super) fn from_agent(agent: &Agent) -> Self {
        Self {
            settings: settings_picker(agent),
            settings_categories: SettingsCategory::ALL
                .into_iter()
                .map(|category| (category, settings_category_picker(agent, category, 0)))
                .collect(),
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

pub(super) fn picker_is_editing(state: &ViewState) -> bool {
    state
        .picker
        .as_ref()
        .is_some_and(|picker| picker.editing.is_some())
}

/// Routes pasted text to an open picker field, or else to the composer.
pub(super) fn paste_into_picker(state: &mut ViewState, editor: &mut Editor, text: &str) {
    match state
        .picker
        .as_mut()
        .and_then(|picker| picker.editing.as_mut())
    {
        Some(PickerEdit::Field(field)) => field.paste(text),
        _ => editor.paste(text),
    }
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

/// Whether this picker lists accent or selection colors.
pub(super) fn picker_is_color_picker(picker: &Picker) -> bool {
    picker.items.iter().any(|item| {
        matches!(
            item.action,
            PickerAction::SetAccentColor(_) | PickerAction::SetSelectionColor(_)
        )
    })
}

/// Keeps the live color preview in step with the open picker: a color picker
/// repaints the interface with its highlighted row, editing a custom value
/// falls back to the committed colors, and any other picker ends the preview
/// without reverting because the selected color is already applied.
pub(super) fn sync_color_preview(state: &mut ViewState) {
    if !state.picker.as_ref().is_some_and(picker_is_color_picker) {
        state.commit_color_preview();
        return;
    }
    if picker_is_editing(state) {
        state.reset_color_preview();
        return;
    }
    let Some(action) = state
        .picker
        .as_ref()
        .and_then(|picker| picker.items.get(picker.selected))
        .map(|item| item.action.clone())
    else {
        return;
    };
    match action {
        PickerAction::SetAccentColor(color) => state.preview_accent_color(color),
        PickerAction::SetSelectionColor(selection) => state.preview_selection_color(selection),
        _ => state.reset_color_preview(),
    }
}

/// What a key did to a picker on its own.
pub(super) enum PickerKey {
    /// The picker stays open.
    Handled,
    /// Esc, or Esc in a standalone field: close and run the parent action.
    Cancel(Option<PickerAction>),
    /// Close and run this action.
    Choose(PickerAction),
}

impl Picker {
    /// The keys every picker shares, in the TUI and on the setup screens:
    /// field editing, list movement, Enter, Esc, and reasoning checkboxes.
    pub(super) fn handle_key(&mut self, key: Key) -> PickerKey {
        if let Some(PickerEdit::Field(field)) = self.editing.as_mut() {
            match field.handle_key(key) {
                field::FieldKey::Submit(action) => return PickerKey::Choose(action),
                // A field opened on its own has no list to return to.
                field::FieldKey::Cancel if self.items.is_empty() => {
                    return PickerKey::Cancel(self.parent.clone());
                }
                field::FieldKey::Cancel => self.editing = None,
                field::FieldKey::Handled => {}
            }
            return PickerKey::Handled;
        }

        if self.edit_filter(key) {
            return PickerKey::Handled;
        }

        if let Some(PickerAction::ConnectToggleReasoning(index)) =
            self.items.get(self.selected).map(|item| &item.action)
        {
            match key {
                Key::Char(' ') => {
                    return PickerKey::Choose(PickerAction::ConnectToggleReasoning(*index));
                }
                Key::Enter => return PickerKey::Choose(PickerAction::ConnectConfirmReasoning),
                _ => {}
            }
        }

        let last = self.items.len().saturating_sub(1);
        match key {
            Key::Escape | Key::Ctrl('c') => return PickerKey::Cancel(self.parent.clone()),
            Key::Up | Key::Char('k') => self.selected = self.selected.saturating_sub(1),
            Key::Down | Key::Char('j') => self.selected = (self.selected + 1).min(last),
            Key::PageUp => self.selected = self.selected.saturating_sub(5),
            Key::PageDown => self.selected = (self.selected + 5).min(last),
            Key::Enter => {
                let Some(item) = self.items.get(self.selected) else {
                    return PickerKey::Handled;
                };
                // Edit rows open their field in place, focused and ready to type.
                if let Some(field) = field::from_item(item) {
                    self.editing = Some(PickerEdit::Field(Box::new(field)));
                    return PickerKey::Handled;
                }
                return PickerKey::Choose(item.action.clone());
            }
            _ => {}
        }
        PickerKey::Handled
    }
}

pub(super) fn take_picker_action(
    state: &mut ViewState,
    editor: &mut Editor,
    key: Key,
) -> Option<PickerAction> {
    let picker = state.picker.as_mut()?;

    if let Some(PickerEdit::Queued(index)) = picker.editing {
        match key {
            Key::Escape | Key::Ctrl('c') => {
                editor.clear();
                picker.editing = None;
            }
            Key::Enter => {
                let input = editor.take_queued_edit()?;
                state.picker = None;
                return Some(PickerAction::ApplyQueued { index, input });
            }
            Key::Up | Key::Down => {}
            _ => {
                let _ = editor.handle_key(key);
            }
        }
        return None;
    }

    if picker.editing.is_none() {
        if picker_is_plan_handoff(picker)
            && let Key::Char(digit @ '1'..='2') = key
        {
            picker.selected = usize::from(digit as u8 - b'1');
            return None;
        }
        // Letters type into a search field when the picker has one.
        if picker.filter.is_none()
            && let Some(action) = row_shortcut(state, editor, key)
        {
            return action;
        }
    }

    let picker = state.picker.as_mut()?;
    match picker.handle_key(key) {
        PickerKey::Handled => None,
        PickerKey::Cancel(action) => {
            state.picker = None;
            // Unconditional: restoring committed colors is a no-op when no
            // preview is live, and it keeps cancel correct if a picker that
            // previews colors ever gains a different shape.
            state.end_color_preview();
            action
        }
        PickerKey::Choose(action) => {
            state.picker = None;
            Some(action)
        }
    }
}

/// TUI-only keys that act on the selected row: `e` edits a queued message,
/// `K`/`J` reorder queued messages and status-bar items, and `d`/Delete
/// removes sessions, models, queued messages, and status-bar items.
/// `Some` means the key was consumed, carrying the action to run, if any.
fn row_shortcut(
    state: &mut ViewState,
    editor: &mut Editor,
    key: Key,
) -> Option<Option<PickerAction>> {
    let selected = state.picker.as_ref()?.selected;
    let action = state.picker.as_ref()?.items.get(selected)?.action.clone();
    if let (Key::Char('e'), PickerAction::SendQueued(index)) = (key, &action) {
        let initial = state.queued_inputs.get(*index)?.clone();
        editor.clear();
        editor.restore_submission(initial);
        state.picker.as_mut()?.editing = Some(PickerEdit::Queued(*index));
        return Some(None);
    }
    let picker = state.picker.as_mut()?;
    match (key, action) {
        (Key::Char(letter @ ('K' | 'J')), action) => {
            let direction = if letter == 'K' { -1 } else { 1 };
            let moved = match action {
                PickerAction::SendQueued(index) => PickerAction::MoveQueued { index, direction },
                PickerAction::OpenStatusBarItem { index, .. } => {
                    PickerAction::MoveStatusBarItem { index, direction }
                }
                _ => return None,
            };
            state.picker = None;
            Some(Some(moved))
        }
        (Key::Delete | Key::Char('d'), action) => match action {
            PickerAction::ResumeSession(id) => {
                let label = picker.items[selected].label.clone();
                let description = picker.items[selected].description.clone();
                *picker = delete_session_confirm(id, label, description, selected);
                Some(None)
            }
            PickerAction::SwitchModel(model) | PickerAction::SaveModel(model) => {
                let save = matches!(picker.items[selected].action, PickerAction::SaveModel(_));
                state.picker = None;
                Some(Some(PickerAction::ConfirmRemoveModel {
                    model,
                    save,
                    selected,
                }))
            }
            PickerAction::SendQueued(index) => Some(Some(PickerAction::RemoveQueued(index))),
            PickerAction::OpenStatusBarItem { index, .. } => {
                Some(Some(PickerAction::RemoveStatusBarItem(index)))
            }
            _ => None,
        },
        _ => None,
    }
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
        filter: None,
        parent: Some(PickerAction::OpenResume {
            selected: resume_selected,
        }),
    }
}

/// Configured provider models can be removed; Codex catalog entries cannot.
pub(super) fn remove_model_confirm(
    config: &Config,
    model: &str,
    save: bool,
    selected: usize,
) -> Result<Picker, String> {
    let configured = model.split_once(':').filter(|(provider, id)| {
        config
            .providers
            .get(*provider)
            .is_some_and(|provider| provider.models.iter().any(|entry| entry.id == *id))
    });
    let Some((provider, _)) = configured else {
        return Err(if crate::model::is_codex(config, model) {
            format!("`{model}` comes from the Codex account catalog and cannot be removed.")
        } else {
            format!("`{model}` is not a configured provider model.")
        });
    };
    let back = PickerAction::ReturnToModels { save, selected };
    Ok(Picker {
        title: "Remove model?".into(),
        hint: "Enter confirm  Esc back".into(),
        selected: 0,
        items: vec![
            PickerItem {
                label: "Cancel".into(),
                description: "Keep this model".into(),
                action: back.clone(),
            },
            PickerItem {
                label: "Remove".into(),
                description: format!("{model} · from providers.{provider}.models"),
                action: PickerAction::RemoveModel {
                    model: model.to_string(),
                    save,
                    selected,
                },
            },
        ],
        editing: None,
        filter: None,
        parent: Some(back),
    })
}

pub(super) fn select_picker_item(state: &mut ViewState, selected: usize) {
    if let Some(picker) = state.picker.as_mut() {
        picker.selected = selected.min(picker.items.len().saturating_sub(1));
    }
}
