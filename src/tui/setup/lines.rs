//! The prompt surface for piped input: each picker as a numbered list and
//! each field as a line prompt, written to stderr so stdout stays clean.

use std::io::{self, Write};
use std::time::Duration;

use crate::error::Error;

use super::super::events::Key;
use super::super::picker::{PickerAction, PickerEdit, PickerKey};
use super::{Page, Surface};

pub(super) struct Lines {
    /// The answer when input ends; `None` makes the end of input an error.
    end_of_input: Option<PickerAction>,
}

impl Lines {
    pub(super) fn new(end_of_input: Option<PickerAction>) -> Self {
        Self { end_of_input }
    }

    fn read(&self) -> Result<Option<String>, Error> {
        io::stderr().flush()?;
        let mut line = String::new();
        match crate::terminal_mode::read_line(&mut line) {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(line.trim().to_string())),
            Err(error) if error.kind() == io::ErrorKind::Interrupted && crate::interrupted() => {
                Err(Error::Interrupted)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn ended(&self) -> Result<PickerAction, Error> {
        self.end_of_input
            .clone()
            .ok_or_else(|| Error::Config("setup canceled: input ended".into()))
    }
}

impl Surface for Lines {
    fn choose(
        &mut self,
        page: &mut Page,
        poll: &mut dyn FnMut(&mut Page) -> bool,
    ) -> Result<PickerAction, Error> {
        loop {
            if page.busy {
                print_list(page);
                while !poll(page) {
                    if crate::interrupted() {
                        return Err(Error::Interrupted);
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                continue;
            }
            if let Some(PickerEdit::Field(field)) = page.picker.editing.as_mut() {
                let initial = field.value().to_string();
                if initial.is_empty() {
                    eprint!("{}: ", field.label());
                } else {
                    eprint!("{} [{initial}]: ", field.label());
                }
                let Some(answer) = self.read()? else {
                    return self.ended();
                };
                // A blank answer keeps the default, or goes back without one.
                let key = if answer.is_empty() && initial.is_empty() {
                    Key::Escape
                } else {
                    if !answer.is_empty() {
                        field.set_value(&answer);
                    }
                    Key::Enter
                };
                match page.picker.handle_key(key) {
                    PickerKey::Cancel(Some(action)) | PickerKey::Choose(action) => {
                        return Ok(action);
                    }
                    PickerKey::Cancel(None) | PickerKey::Handled => {
                        if let Some(PickerEdit::Field(field)) = &page.picker.editing {
                            eprintln!("{}", field.message());
                        }
                    }
                }
                continue;
            }

            print_list(page);
            let toggles = page
                .picker
                .items
                .iter()
                .any(|item| matches!(item.action, PickerAction::ConnectToggleReasoning(_)));
            let searchable = page.picker.filter.is_some();
            eprint!(
                "{}",
                if toggles {
                    "Number to toggle, Enter to confirm, or 'q' to go back: "
                } else if searchable {
                    "Number, text to search, or Enter to go back: "
                } else {
                    "Number, or Enter to go back: "
                }
            );
            let Some(answer) = self.read()? else {
                return self.ended();
            };
            let key = match answer.as_str() {
                "" if toggles => Key::Enter,
                "" | "q" => Key::Escape,
                number => match number
                    .parse::<usize>()
                    .ok()
                    .and_then(|number| number.checked_sub(1))
                    .filter(|index| *index < page.picker.items.len())
                {
                    Some(index) => {
                        page.picker.selected = index;
                        if toggles { Key::Char(' ') } else { Key::Enter }
                    }
                    None if searchable => {
                        // Any other text narrows the list; the next prompt
                        // numbers the matches.
                        page.picker.set_filter(number);
                        continue;
                    }
                    None => {
                        eprintln!("Enter a number from 1 through {}.", page.picker.items.len());
                        continue;
                    }
                },
            };
            match page.picker.handle_key(key) {
                PickerKey::Cancel(Some(action)) | PickerKey::Choose(action) => return Ok(action),
                // The row opened its field; the loop prompts for it.
                PickerKey::Cancel(None) | PickerKey::Handled => {}
            }
        }
    }
}

fn print_list(page: &Page) {
    let mut lines = vec![String::new(), page.picker.title.clone()];
    if let Some(notice) = &page.notice {
        lines.push(notice.clone());
    }
    if let Some(filter) = page.picker.filter.as_ref().filter(|f| !f.query.is_empty()) {
        let (matched, total) = filter.counts(page.picker.items.len());
        lines.push(format!(
            "Search \"{}\": {matched} of {total} (Enter clears)",
            filter.query
        ));
    }
    for (index, item) in page.picker.items.iter().enumerate() {
        if page.busy {
            lines.push(format!("  {} · {}", item.label, item.description));
        } else if item.description.is_empty() {
            lines.push(format!("  {}. {}", index + 1, item.label));
        } else {
            lines.push(format!(
                "  {}. {} · {}",
                index + 1,
                item.label,
                item.description
            ));
        }
    }
    for line in lines {
        eprintln!(
            "{}",
            crate::text::sanitize_line(&line, crate::text::Tabs::Space)
        );
    }
}
