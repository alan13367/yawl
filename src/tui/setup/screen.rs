//! The full-screen surface: the wordmark, a tagline and details, setup
//! progress, and the picker panel, on the alternate screen.

use std::io::{self, Write};

use crate::config::{Config, Theme, UiColor};
use crate::error::Error;
use crate::terminal_mode::RawMode;

use super::super::events::{Event, EventReader, Key};
use super::super::input::Editor;
use super::super::markdown;
use super::super::picker::{PickerAction, PickerEdit, PickerKey, render_picker};
use super::super::remote::TerminalInput;
use super::super::render::{foreground_color, render_wordmark, selection_style, status_style};
use super::super::terminal::{
    cursor_control, query_light_background, terminal_size, write_frame_rows,
};
use super::super::theme;
use super::{Backdrop, Page, SETUP_STEPS, Surface};

pub(super) struct Screen {
    // Declared first so the terminal leaves raw mode only after `Drop`
    // below has restored the main screen.
    _raw_mode: RawMode,
    stdout: io::Stdout,
    events: EventReader<TerminalInput>,
    accent: UiColor,
    selection: UiColor,
    /// Pickers render a queued-message draft from the composer; setup
    /// has none, so this stays empty.
    editor: Editor,
    previous: Vec<String>,
    last_cursor: String,
    size: (u16, u16),
    /// Idle ticks since the screen opened, which drive the wordmark typing.
    tick: usize,
}

impl Screen {
    pub(super) fn enter(config: &Config) -> Result<Self, Error> {
        let raw_mode = RawMode::enter()?;
        // Ctrl+C arrives as a key, so the screen is restored before the
        // interruption is reported.
        raw_mode.set_signals(false)?;
        let mut stdout = io::stdout();
        stdout.write_all(b"\x1b[?1049h\x1b[2J\x1b[H\x1b[?25l\x1b[?2004h")?;
        stdout.flush()?;
        // An explicit theme needs no answer from the terminal.
        let (light_background, typed) = if config.theme == Theme::Auto {
            query_light_background(&mut stdout)
        } else {
            (None, Vec::new())
        };
        theme::init(config.theme, light_background);
        Ok(Self {
            _raw_mode: raw_mode,
            stdout,
            events: EventReader::with_pending(TerminalInput::stdin(), &typed),
            accent: config.accent_color,
            selection: config.selection_color.unwrap_or(config.accent_color),
            editor: Editor::default(),
            previous: Vec::new(),
            last_cursor: String::new(),
            size: (0, 0),
            tick: 0,
        })
    }

    fn draw(&mut self, page: &Page) -> Result<(), Error> {
        let (columns, rows) = terminal_size();
        let force = (columns, rows) != self.size;
        if force {
            self.size = (columns, rows);
            self.stdout.write_all(b"\x1b[2J")?;
        }
        let (frame, cursor) = compose(
            page,
            &self.editor,
            Colors {
                accent: self.accent,
                selection: self.selection,
            },
            usize::from(columns),
            usize::from(rows),
            self.tick,
        );
        let changed = write_frame_rows(&mut self.stdout, &frame, &self.previous, force)?;
        // The terminal counts from one; zero hides the cursor.
        let cursor = cursor.map_or((0, 0), |(row, column)| (row + 1, column + 1));
        let cursor = cursor_control(cursor, false);
        if changed || cursor != self.last_cursor {
            self.stdout.write_all(cursor.as_bytes())?;
            self.stdout.flush()?;
            self.last_cursor = cursor;
        }
        self.previous = frame;
        Ok(())
    }
}

impl Surface for Screen {
    fn choose(
        &mut self,
        page: &mut Page,
        poll: &mut dyn FnMut(&mut Page) -> bool,
    ) -> Result<PickerAction, Error> {
        loop {
            self.draw(page)?;
            let event = match self.events.read_event() {
                Ok(event) => event,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => Event::Tick,
                Err(error) => return Err(error.into()),
            };
            match event {
                Event::Key(Key::Ctrl('c')) => return Err(Error::Interrupted),
                Event::Key(key) => match page.picker.handle_key(key) {
                    PickerKey::Handled | PickerKey::Cancel(None) => {}
                    PickerKey::Cancel(Some(action)) | PickerKey::Choose(action) => {
                        return Ok(action);
                    }
                },
                Event::Paste(text) => {
                    if let Some(PickerEdit::Field(field)) = page.picker.editing.as_mut() {
                        field.paste(&text);
                    }
                }
                Event::Tick => {
                    self.tick = self.tick.saturating_add(1);
                    poll(page);
                }
                _ => {}
            }
        }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = self
            .stdout
            .write_all(b"\x1b[?2004l\x1b[?25h\x1b[0m\x1b[?1049l");
        let _ = self.stdout.flush();
    }
}

#[derive(Clone, Copy)]
struct Colors {
    accent: UiColor,
    selection: UiColor,
}

/// Lays out one frame: the backdrop above the picker panel and the notice
/// below it, centered together, dropping the wordmark and then the details
/// when the terminal is short. Returns the rows and the zero-based cursor.
fn compose(
    page: &Page,
    editor: &Editor,
    colors: Colors,
    columns: usize,
    rows: usize,
    tick: usize,
) -> (Vec<String>, Option<(usize, usize)>) {
    let blank = " ".repeat(columns);
    let outline = foreground_color(colors.accent);
    let (mut panel, mut cursor) = render_picker(
        &page.picker,
        editor,
        &selection_style(colors.selection),
        &outline,
        columns,
        rows,
    );
    // The picker centers itself in the rows it is given; keep only its box.
    let top = panel.iter().take_while(|line| **line == blank).count();
    panel.drain(..top);
    while panel.last().is_some_and(|line| *line == blank) {
        panel.pop();
    }
    cursor = cursor.and_then(|(row, column)| Some((row.checked_sub(top)?, column)));

    let mut footer = Vec::new();
    if let Some(notice) = &page.notice {
        footer.push(center(
            &crate::text::sanitize_line(notice, crate::text::Tabs::Space),
            &status_style(colors.accent),
            columns,
        ));
    }
    let room = rows.saturating_sub(panel.len() + footer.len());
    let header = header(&page.backdrop, colors.accent, columns, room, tick);

    let content = header.len() + panel.len() + footer.len();
    let top = rows.saturating_sub(content) / 2;
    cursor = cursor.map(|(row, column)| (top + header.len() + row, column));
    let mut frame = vec![blank.clone(); top];
    frame.extend(header);
    frame.extend(panel);
    frame.extend(footer);
    frame.resize(rows, blank);
    frame.truncate(rows);
    (frame, cursor.filter(|(row, _)| *row < rows))
}

/// The tallest backdrop that fits `room` rows: wordmark, text, and progress;
/// then text and progress; then the tagline alone.
fn header(
    backdrop: &Backdrop,
    accent: UiColor,
    columns: usize,
    room: usize,
    tick: usize,
) -> Vec<String> {
    let blank = " ".repeat(columns);
    let mut text = vec![center(
        &backdrop.tagline,
        &format!("{}\x1b[1m", foreground_color(accent)),
        columns,
    )];
    // Details wrap, so a long project path stays readable.
    text.extend(
        backdrop
            .details
            .iter()
            .flat_map(|line| markdown::wrapped_plain_lines(line, columns.saturating_sub(4).max(1)))
            .map(|line| center(&line, "\x1b[2m", columns)),
    );
    if let Some(current) = backdrop.progress {
        text.push(blank.clone());
        text.push(progress_line(current, accent, columns));
    }
    text.push(blank.clone());

    let (logo, _) = render_wordmark(accent, columns, room, tick);
    let mut full = logo;
    full.push(blank.clone());
    full.extend(text.iter().cloned());
    if full.len() <= room {
        return full;
    }
    if text.len() <= room {
        return text;
    }
    if room >= 2 {
        return vec![text[0].clone(), blank];
    }
    Vec::new()
}

/// `✓ Provider ── ● Connect ── ○ Model ── ○ Save`, or a short form when
/// that does not fit.
fn progress_line(current: usize, accent: UiColor, columns: usize) -> String {
    let done = status_style(accent);
    let active = format!("{}\x1b[1m", foreground_color(accent));
    let steps = SETUP_STEPS
        .iter()
        .enumerate()
        .map(|(index, step)| match index.cmp(&current) {
            std::cmp::Ordering::Less => format!("{done}✓ {step}\x1b[0m"),
            std::cmp::Ordering::Equal => format!("{active}● {step}\x1b[0m"),
            std::cmp::Ordering::Greater => format!("\x1b[2m○ {step}\x1b[0m"),
        })
        .collect::<Vec<_>>()
        .join("\x1b[2m ── \x1b[0m");
    if markdown::visible_width(&steps) <= columns {
        return center(&steps, "", columns);
    }
    let short = match SETUP_STEPS.get(current) {
        Some(step) => format!("Step {} of {} · {step}", current + 1, SETUP_STEPS.len()),
        None => "Done".to_string(),
    };
    center(&short, &done, columns)
}

fn center(text: &str, style: &str, width: usize) -> String {
    let left = width.saturating_sub(markdown::visible_width(text)) / 2;
    markdown::fit_width(&format!("{}{style}{text}\x1b[0m", " ".repeat(left)), width)
}

#[cfg(test)]
mod tests {
    use super::super::super::picker::{Picker, PickerItem};
    use super::*;

    fn page(notice: Option<&str>) -> Page {
        Page {
            backdrop: Backdrop {
                tagline: "Welcome to Yawl".into(),
                details: vec!["Connect a model provider to get started.".into()],
                progress: Some(1),
            },
            picker: Picker {
                title: "Choose a provider".into(),
                hint: "Enter select".into(),
                items: vec![PickerItem {
                    label: "Ollama".into(),
                    description: "local models".into(),
                    action: PickerAction::Choice(0),
                }],
                selected: 0,
                editing: None,
                parent: None,
            },
            notice: notice.map(str::to_string),
            busy: false,
        }
    }

    const COLORS: Colors = Colors {
        accent: UiColor::WHITE,
        selection: UiColor::WHITE,
    };

    fn plain(frame: &[String]) -> Vec<String> {
        frame
            .iter()
            .map(|line| markdown::strip_ansi(line))
            .collect()
    }

    #[test]
    fn tall_screens_show_the_wordmark_progress_and_panel_centered() {
        let (frame, cursor) = compose(
            &page(Some("Saved")),
            &Editor::default(),
            COLORS,
            80,
            40,
            100,
        );
        assert_eq!(frame.len(), 40);
        assert!(frame.iter().all(|line| markdown::visible_width(line) == 80));
        let text = plain(&frame).join("\n");
        assert!(text.contains("██"), "{text}");
        assert!(text.contains("✓ Provider ── ● Connect ── ○ Model ── ○ Save"));
        assert!(text.contains("Choose a provider"));
        assert!(text.contains("Saved"));
        assert_eq!(cursor, None);
        assert!(plain(&frame)[0].trim().is_empty());
    }

    #[test]
    fn short_screens_drop_the_wordmark_before_the_panel() {
        let (frame, _) = compose(&page(None), &Editor::default(), COLORS, 80, 16, 100);
        let text = plain(&frame).join("\n");
        assert!(!text.contains("██"));
        assert!(text.contains("Welcome to Yawl"));
        assert!(text.contains("Ollama"));
    }

    #[test]
    fn long_details_wrap_instead_of_running_off_the_screen() {
        let mut page = page(None);
        page.backdrop.details = vec![format!("/srv/{}/project", "deep".repeat(30))];
        let (frame, _) = compose(&page, &Editor::default(), COLORS, 60, 40, 100);
        let text = plain(&frame).join("");
        assert!(
            text.contains("/srv/") && text.contains("/project"),
            "{text}"
        );
    }

    #[test]
    fn narrow_screens_shorten_progress() {
        let line = markdown::strip_ansi(&progress_line(1, UiColor::WHITE, 30));
        assert_eq!(line.trim(), "Step 2 of 4 · Connect");
        let done = markdown::strip_ansi(&progress_line(4, UiColor::WHITE, 30));
        assert_eq!(done.trim(), "Done");
    }

    #[test]
    fn a_focused_field_places_the_cursor_inside_the_frame() {
        let mut page = page(None);
        page.picker = super::super::super::picker::field_picker(
            "Ollama endpoint",
            super::super::super::connection::field(
                super::super::super::connection::ConnectEditField::Endpoint,
                "http://127.0.0.1:11434/v1".into(),
            ),
            PickerAction::Choice(0),
        );
        let (frame, cursor) = compose(&page, &Editor::default(), COLORS, 80, 30, 100);
        let (row, column) = cursor.expect("field cursor");
        assert!(row < frame.len() && column < 80);
        assert!(markdown::strip_ansi(&frame[row]).contains("11434"));
    }
}
