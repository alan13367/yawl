//! Single-line text fields inside pickers: provider names, endpoints, API
//! keys, model IDs, settings values, and status-bar labels.
//!
//! A field owns its text, so typing never touches the composer draft. It
//! opens focused with its cursor in place, masks secrets behind a Tab
//! toggle, and shows validation problems under the value instead of in the
//! transcript.

use unicode_width::UnicodeWidthChar;

use super::{Picker, PickerAction, PickerEdit, PickerItem, SettingsLocation};
use crate::tui::connection::ConnectEditField;
use crate::tui::events::Key;
use crate::tui::input::{line_edit, line_insert};
use crate::tui::{markdown, theme};

/// Wrapped help or error rows under the field.
const MESSAGE_ROWS: usize = 2;

/// Where a submitted value goes.
#[derive(Clone)]
pub(in crate::tui) enum FieldTarget {
    Setting {
        key: String,
        location: Option<SettingsLocation>,
    },
    Model {
        save: bool,
    },
    StatusBarLabel(usize),
    StatusBarSeparator,
    Connect(ConnectEditField),
}

#[derive(Clone)]
pub(in crate::tui) struct PickerField {
    target: FieldTarget,
    label: String,
    help: String,
    placeholder: String,
    value: String,
    cursor: usize,
    secret: bool,
    revealed: bool,
    error: Option<String>,
}

/// What a key did to a focused field.
pub(in crate::tui) enum FieldKey {
    Submit(PickerAction),
    Cancel,
    Handled,
}

impl PickerField {
    pub(in crate::tui) fn new(
        target: FieldTarget,
        label: impl Into<String>,
        help: impl Into<String>,
        initial: impl Into<String>,
    ) -> Self {
        let value = clean(&initial.into());
        Self {
            target,
            label: label.into(),
            help: help.into(),
            placeholder: String::new(),
            cursor: value.chars().count(),
            value,
            secret: false,
            revealed: false,
            error: None,
        }
    }

    /// Masks the value; Tab shows or hides it.
    pub(in crate::tui) fn secret(mut self) -> Self {
        self.secret = true;
        self
    }

    /// Dim example text shown while the field is empty.
    pub(in crate::tui) fn placeholder(mut self, text: impl Into<String>) -> Self {
        self.placeholder = text.into();
        self
    }

    /// Shows `error` under the value until the next edit.
    pub(in crate::tui) fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }

    pub(in crate::tui) fn value(&self) -> &str {
        &self.value
    }

    pub(in crate::tui) fn label(&self) -> &str {
        &self.label
    }

    /// The validation problem, or else the help text.
    pub(in crate::tui) fn message(&self) -> &str {
        self.error.as_deref().unwrap_or(&self.help)
    }

    /// Replaces the value, as a line prompt does with what was typed.
    pub(in crate::tui) fn set_value(&mut self, text: &str) {
        self.value = clean(text);
        self.cursor = self.value.chars().count();
        self.error = None;
    }

    /// Inserts pasted text at the cursor. Newlines and control characters
    /// are dropped, so a key copied with a trailing newline pastes cleanly.
    pub(in crate::tui) fn paste(&mut self, text: &str) {
        line_insert(&mut self.value, &mut self.cursor, &clean(text));
        self.error = None;
    }

    pub(in crate::tui) fn handle_key(&mut self, key: Key) -> FieldKey {
        match key {
            Key::Escape | Key::Ctrl('c') => FieldKey::Cancel,
            Key::Enter => match self.submit() {
                Ok(action) => FieldKey::Submit(action),
                Err(error) => {
                    self.error = Some(error);
                    FieldKey::Handled
                }
            },
            Key::Tab if self.secret => {
                self.revealed = !self.revealed;
                FieldKey::Handled
            }
            key => {
                if line_edit(&mut self.value, &mut self.cursor, key) {
                    self.error = None;
                }
                FieldKey::Handled
            }
        }
    }

    fn submit(&self) -> Result<PickerAction, String> {
        let trimmed = self.value.trim().to_string();
        match &self.target {
            // Labels and separators keep their spacing; empty clears them.
            FieldTarget::StatusBarLabel(index) => Ok(PickerAction::ApplyStatusBarLabel {
                index: *index,
                label: self.value.clone(),
            }),
            FieldTarget::StatusBarSeparator => {
                Ok(PickerAction::ApplyStatusBarSeparator(self.value.clone()))
            }
            _ if trimmed.is_empty() => Err(format!(
                "Enter {}, or press Esc to go back.",
                with_article(&self.label)
            )),
            FieldTarget::Setting { key, location } => Ok(PickerAction::ApplySetting {
                argument: format!("{key} {trimmed}"),
                location: *location,
            }),
            FieldTarget::Model { save: true } => Ok(PickerAction::SaveModel(trimmed)),
            FieldTarget::Model { save: false } => Ok(PickerAction::SwitchModel(trimmed)),
            FieldTarget::Connect(field) => Ok(PickerAction::ApplyConnect {
                field: *field,
                value: trimmed,
            }),
        }
    }

    pub(in crate::tui) fn hint(&self, standalone: bool) -> &'static str {
        match (self.secret, standalone) {
            (true, true) => "Enter continue  Tab show/hide  Esc back",
            (true, false) => "Enter save  Tab show/hide  Esc cancel",
            (false, true) => "Enter continue  Esc back",
            (false, false) => "Enter save  Esc cancel",
        }
    }

    /// Panel rows for an `inner`-wide picker body, plus the row and column
    /// of the cursor within them.
    pub(in crate::tui) fn render(
        &self,
        inner: usize,
        outline: &str,
    ) -> (Vec<String>, (usize, usize)) {
        // One column of margin on each side of the box.
        let box_rule = "─".repeat(inner.saturating_sub(4));
        let text_width = inner.saturating_sub(6).max(1);
        let (text, cursor_col) = self.visible_text(text_width);
        let padding = " ".repeat(text_width.saturating_sub(markdown::visible_width(&text)));
        let mut rows = vec![
            String::new(),
            format!(" \x1b[1m{}\x1b[0m", self.label),
            format!(" {outline}╭{box_rule}╮\x1b[0m"),
            format!(" {outline}│\x1b[0m {text}{padding} {outline}│\x1b[0m"),
            format!(" {outline}╰{box_rule}╯\x1b[0m"),
        ];
        let cursor = (3, 3 + cursor_col);
        let (message, color) = match &self.error {
            Some(error) => (format!("✗ {error}"), theme::palette().field_error),
            None => (self.help.clone(), "\x1b[2m"),
        };
        let mut lines = markdown::wrapped_plain_lines(&message, inner.saturating_sub(2).max(1));
        lines.truncate(MESSAGE_ROWS);
        lines.resize(MESSAGE_ROWS, String::new());
        rows.extend(
            lines
                .into_iter()
                .map(|line| format!(" {color}{line}\x1b[0m")),
        );
        rows.push(String::new());
        (rows, cursor)
    }

    /// The part of the value that fits `width` columns, scrolled so the
    /// cursor stays visible, with the cursor's column inside it.
    fn visible_text(&self, width: usize) -> (String, usize) {
        if self.value.is_empty() {
            let placeholder = markdown::fit_width(&self.placeholder, width);
            return (format!("\x1b[2m{}\x1b[0m", placeholder.trim_end()), 0);
        }
        let shown: Vec<char> = if self.secret && !self.revealed {
            vec!['•'; self.value.chars().count()]
        } else {
            self.value.chars().collect()
        };
        let cursor = self.cursor.min(shown.len());
        let columns = |chars: &[char]| -> usize {
            chars
                .iter()
                .map(|character| character.width().unwrap_or(0))
                .sum()
        };
        // Leave one cell for the cursor itself at the end of the value.
        let mut start = 0;
        while start < cursor && columns(&shown[start..cursor]) >= width {
            start += 1;
        }
        let mut text = String::new();
        let mut used = 0;
        for character in &shown[start..] {
            let cell = character.width().unwrap_or(0);
            if used + cell > width {
                break;
            }
            text.push(*character);
            used += cell;
        }
        (text, columns(&shown[start..cursor]))
    }
}

fn clean(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control())
        .collect()
}

/// "API key" → "an API key", "Model ID" → "a model ID": lowercases only a
/// leading word that is not an acronym.
fn with_article(label: &str) -> String {
    let (first, rest) = label.split_once(' ').unwrap_or((label, ""));
    let acronym = first.len() > 1
        && first
            .chars()
            .all(|character| character.is_ascii_uppercase());
    let first = if acronym {
        first.to_string()
    } else {
        first.to_lowercase()
    };
    let article = if first.starts_with(['a', 'e', 'i', 'o', 'u', 'A', 'E', 'I', 'O', 'U']) {
        "an"
    } else {
        "a"
    };
    if rest.is_empty() {
        format!("{article} {first}")
    } else {
        format!("{article} {first} {rest}")
    }
}

/// A picker that is only a focused field. Esc goes to `parent`.
pub(in crate::tui) fn field_picker(
    title: impl Into<String>,
    field: PickerField,
    parent: PickerAction,
) -> Picker {
    Picker {
        title: title.into(),
        hint: String::new(),
        items: Vec::new(),
        selected: 0,
        editing: Some(PickerEdit::Field(Box::new(field))),
        parent: Some(parent),
    }
}

/// The field a list row's edit action opens, labeled from that row. The row
/// description becomes help text unless it only repeats the current value.
pub(super) fn from_item(item: &PickerItem) -> Option<PickerField> {
    let label = item.label.trim_end_matches('…').trim();
    let help = |initial: &str| {
        if item.description == initial {
            String::new()
        } else {
            item.description.clone()
        }
    };
    let field = match &item.action {
        PickerAction::EditSetting {
            key,
            initial,
            location,
        } => PickerField::new(
            FieldTarget::Setting {
                key: key.clone(),
                location: *location,
            },
            label,
            help(initial),
            initial.clone(),
        ),
        PickerAction::EditSecretSetting { key, location } => PickerField::new(
            FieldTarget::Setting {
                key: key.clone(),
                location: *location,
            },
            label,
            item.description.clone(),
            String::new(),
        )
        .secret()
        .placeholder("Paste the key"),
        PickerAction::EditModel { save, initial } => PickerField::new(
            FieldTarget::Model { save: *save },
            "Model ID",
            "The exact ID, with a provider prefix such as openai: when needed",
            initial.clone(),
        ),
        PickerAction::EditConnect {
            field,
            initial,
            secret,
        } => {
            let field = crate::tui::connection::field(*field, initial.clone());
            if *secret { field.secret() } else { field }
        }
        PickerAction::EditStatusBarLabel { index, initial } => PickerField::new(
            FieldTarget::StatusBarLabel(*index),
            label,
            "Leave empty to use the built-in label",
            initial.clone(),
        ),
        PickerAction::EditStatusBarSeparator(initial) => PickerField::new(
            FieldTarget::StatusBarSeparator,
            label,
            "Text placed between status-bar items; spaces count",
            initial.clone(),
        ),
        _ => return None,
    };
    Some(field)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(target: FieldTarget, initial: &str) -> PickerField {
        PickerField::new(target, "Name", "help", initial)
    }

    fn type_text(field: &mut PickerField, text: &str) {
        for character in text.chars() {
            field.handle_key(Key::Char(character));
        }
    }

    #[test]
    fn typing_edits_in_place_and_enter_submits_the_trimmed_value() {
        let mut name = field(FieldTarget::Connect(ConnectEditField::ProviderName), "");
        type_text(&mut name, "  my-provider ");
        let FieldKey::Submit(PickerAction::ApplyConnect { field, value }) =
            name.handle_key(Key::Enter)
        else {
            panic!("expected a connect submission");
        };
        assert_eq!(field, ConnectEditField::ProviderName);
        assert_eq!(value, "my-provider");
    }

    #[test]
    fn empty_values_explain_themselves_and_clear_on_the_next_edit() {
        let mut model = PickerField::new(FieldTarget::Model { save: false }, "Model ID", "", "");
        assert!(matches!(model.handle_key(Key::Enter), FieldKey::Handled));
        let (rows, _) = model.render(40, "");
        let text = markdown::strip_ansi(&rows.join("\n"));
        assert!(text.contains("✗ Enter a model ID, or press Esc"), "{text}");

        model.handle_key(Key::Char('x'));
        let (rows, _) = model.render(40, "");
        assert!(!markdown::strip_ansi(&rows.join("\n")).contains('✗'));

        let mut label = field(FieldTarget::StatusBarLabel(2), "Ctx");
        label.handle_key(Key::Ctrl('u'));
        assert!(matches!(
            label.handle_key(Key::Enter),
            FieldKey::Submit(PickerAction::ApplyStatusBarLabel { index: 2, label }) if label.is_empty()
        ));
    }

    #[test]
    fn secrets_are_masked_until_tab_reveals_them() {
        let mut key = field(FieldTarget::Connect(ConnectEditField::Secret), "").secret();
        key.paste("sk-test-123\n");
        assert_eq!(key.value(), "sk-test-123");
        let masked = markdown::strip_ansi(&key.render(40, "").0.join("\n"));
        assert!(!masked.contains("sk-test"));
        assert!(masked.contains(&"•".repeat(11)));

        key.handle_key(Key::Tab);
        let shown = markdown::strip_ansi(&key.render(40, "").0.join("\n"));
        assert!(shown.contains("sk-test-123"));
        key.handle_key(Key::Tab);
        assert!(!markdown::strip_ansi(&key.render(40, "").0.join("\n")).contains("sk-test"));
    }

    #[test]
    fn long_values_scroll_to_keep_the_cursor_visible() {
        let url = format!("https://{}.example.com/v1", "x".repeat(60));
        let mut endpoint = field(FieldTarget::Connect(ConnectEditField::Endpoint), &url);
        let (rows, (row, column)) = endpoint.render(30, "");
        let value_row = markdown::strip_ansi(&rows[row]);
        assert!(value_row.contains("example.com/v1"), "{value_row}");
        assert!(column < 30);
        assert_eq!(
            markdown::visible_width(&rows[row]),
            29,
            "one column of right margin"
        );

        endpoint.handle_key(Key::Home);
        let (rows, (row, column)) = endpoint.render(30, "");
        assert!(markdown::strip_ansi(&rows[row]).contains("https://"));
        assert_eq!(column, 3);
    }

    #[test]
    fn placeholders_show_only_while_empty() {
        let mut name = field(FieldTarget::Connect(ConnectEditField::ProviderName), "")
            .placeholder("my-provider");
        let (rows, (row, column)) = name.render(40, "");
        assert!(markdown::strip_ansi(&rows[row]).contains("my-provider"));
        assert_eq!(column, 3);
        type_text(&mut name, "q");
        let (rows, (row, _)) = name.render(40, "");
        assert!(!markdown::strip_ansi(&rows[row]).contains("my-provider"));
    }
}
