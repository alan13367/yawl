//! Raw terminal lifecycle, frame output, selection, and clipboard support.

use std::io::{self, IsTerminal, Write};

use crate::error::Error;

use super::events::{MouseEvent, MouseKind};
use super::input::Editor;
use super::render::build_frame;
use super::{ViewState, markdown};

pub(super) struct Terminal {
    original: libc::termios,
    stdout: io::Stdout,
    active: bool,
    last_frame: Vec<String>,
    last_base_frame: Vec<String>,
    last_size: (u16, u16),
    selection: Option<TextSelection>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ScreenPoint {
    pub(super) row: usize,
    pub(super) column: usize,
}

pub(super) struct TextSelection {
    pub(super) anchor: ScreenPoint,
    pub(super) current: ScreenPoint,
    pub(super) frame: Vec<String>,
}

impl Terminal {
    pub(super) fn enter() -> Result<Self, Error> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(Error::Config("the terminal UI needs a TTY".into()));
        }
        // SAFETY: A zeroed termios value is immediately initialized by
        // `tcgetattr` before any field is read.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: STDIN_FILENO is valid for this process and `original`
        // points to writable termios storage.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        let mut raw = original;
        // SAFETY: `raw` is an initialized termios value.
        unsafe { libc::cfmakeraw(&mut raw) };
        // Keep legacy Ctrl+C as SIGINT. Disable the other signal-generating
        // control characters because suspending would leave the terminal in
        // raw mode.
        raw.c_lflag |= libc::ISIG;
        raw.c_cc[libc::VQUIT] = 0;
        raw.c_cc[libc::VSUSP] = 0;
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 1;
        // SAFETY: STDIN_FILENO is valid and `raw` points to initialized
        // termios storage for this terminal.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw) } != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }

        let mut terminal = Self {
            original,
            stdout: io::stdout(),
            active: true,
            last_frame: Vec::new(),
            last_base_frame: Vec::new(),
            last_size: (0, 0),
            selection: None,
        };
        terminal.stdout.write_all(
            b"\x1b[?1049h\x1b[2J\x1b[H\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[>1u",
        )?;
        terminal.stdout.flush()?;
        Ok(terminal)
    }

    pub(super) fn invalidate(&mut self) {
        self.last_frame.clear();
        self.last_base_frame.clear();
        self.last_size = (0, 0);
    }

    pub(super) fn handle_mouse(&mut self, event: MouseEvent) -> Result<bool, Error> {
        let point = ScreenPoint {
            row: event.row,
            column: event.column,
        };
        match event.kind {
            MouseKind::Press => {
                if self.last_base_frame.is_empty() {
                    return Ok(false);
                }
                let point = clamp_point(point, &self.last_base_frame);
                self.selection = Some(TextSelection {
                    anchor: point,
                    current: point,
                    frame: self.last_base_frame.clone(),
                });
                Ok(false)
            }
            MouseKind::Drag => {
                if let Some(selection) = self.selection.as_mut() {
                    selection.current = clamp_point(point, &selection.frame);
                }
                Ok(false)
            }
            MouseKind::Release => {
                let Some(mut selection) = self.selection.take() else {
                    return Ok(false);
                };
                selection.current = clamp_point(point, &selection.frame);
                let text = selected_text(&selection);
                if text.is_empty() {
                    return Ok(false);
                }
                if !copy_with_platform_command(&text) {
                    let encoded = base64_encode(text.as_bytes());
                    write!(self.stdout, "\x1b]52;c;{encoded}\x07")?;
                    self.stdout.flush()?;
                }
                Ok(true)
            }
        }
    }

    pub(super) fn draw(&mut self, state: &mut ViewState, editor: &Editor) -> Result<(), Error> {
        let (columns, rows) = terminal_size();
        let (base_frame, cursor) =
            build_frame(state, editor, usize::from(columns), usize::from(rows));
        self.last_base_frame.clone_from(&base_frame);
        let frame = self
            .selection
            .as_ref()
            .map_or(base_frame, highlighted_selection);
        let force = self.last_size != (columns, rows) || self.last_frame.len() != frame.len();
        self.stdout.write_all(b"\x1b[?25l")?;
        if force {
            self.stdout.write_all(b"\x1b[2J")?;
        }
        for (index, line) in frame.iter().enumerate() {
            if force || self.last_frame.get(index) != Some(line) {
                write!(self.stdout, "\x1b[{};1H\x1b[2K{line}", index + 1)?;
            }
        }
        if self.selection.is_some() {
            self.stdout.write_all(b"\x1b[?25l")?;
        } else {
            write!(self.stdout, "\x1b[{};{}H\x1b[?25h", cursor.0, cursor.1)?;
        }
        self.stdout.flush()?;
        self.last_frame = frame;
        self.last_size = (columns, rows);
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let _ = self.stdout.write_all(
            b"\x1b[<u\x1b[?2004l\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?25h\x1b[0m\x1b[?1049l",
        );
        let _ = self.stdout.flush();
        // SAFETY: `original` came from a successful `tcgetattr` call for
        // STDIN_FILENO and remains initialized for the life of this guard.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.original);
        }
        self.active = false;
    }
}

pub(super) fn clamp_point(point: ScreenPoint, frame: &[String]) -> ScreenPoint {
    let row = point.row.min(frame.len().saturating_sub(1));
    let width = frame
        .get(row)
        .map_or(1, |line| markdown::visible_width(line).max(1));
    ScreenPoint {
        row,
        column: point.column.min(width.saturating_sub(1)),
    }
}

pub(super) fn selected_text(selection: &TextSelection) -> String {
    if selection.anchor == selection.current {
        return String::new();
    }
    let (start, end) = if selection.anchor <= selection.current {
        (selection.anchor, selection.current)
    } else {
        (selection.current, selection.anchor)
    };
    selection.frame[start.row..=end.row]
        .iter()
        .enumerate()
        .map(|(offset, line)| {
            let plain = markdown::strip_ansi(line);
            let line_length = plain.chars().count();
            let row = start.row + offset;
            let from = if row == start.row { start.column } else { 0 };
            let through = if row == end.row {
                end.column.saturating_add(1)
            } else {
                line_length
            };
            plain
                .chars()
                .skip(from.min(line_length))
                .take(
                    through
                        .min(line_length)
                        .saturating_sub(from.min(line_length)),
                )
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end_matches('\n')
        .to_string()
}

pub(super) fn highlighted_selection(selection: &TextSelection) -> Vec<String> {
    let (start, end) = if selection.anchor <= selection.current {
        (selection.anchor, selection.current)
    } else {
        (selection.current, selection.anchor)
    };
    let mut frame = selection.frame.clone();
    for (row, line) in frame
        .iter_mut()
        .enumerate()
        .take(end.row + 1)
        .skip(start.row)
    {
        let width = markdown::visible_width(line);
        let from = if row == start.row { start.column } else { 0 };
        let through = if row == end.row {
            end.column.saturating_add(1)
        } else {
            width
        };
        *line = highlight_cells(line, from.min(width), through.min(width));
    }
    frame
}

pub(super) fn highlight_cells(line: &str, from: usize, through: usize) -> String {
    if from >= through {
        return line.to_string();
    }
    let mut output = String::with_capacity(line.len() + 16);
    let mut index = 0usize;
    let mut column = 0usize;
    let mut highlighted = false;
    while index < line.len() {
        if column == from && !highlighted {
            output.push_str("\x1b[7m");
            highlighted = true;
        }
        if column == through && highlighted {
            output.push_str("\x1b[27m");
            highlighted = false;
        }
        if line.as_bytes()[index] == 0x1b {
            let search_start = (index + 2).min(line.len());
            let end = line.as_bytes()[search_start..]
                .iter()
                .position(|byte| (0x40..=0x7e).contains(byte))
                .map_or(line.len(), |relative| search_start + relative + 1);
            output.push_str(&line[index..end]);
            if highlighted && line.as_bytes().get(end.saturating_sub(1)) == Some(&b'm') {
                output.push_str("\x1b[7m");
            }
            index = end;
            continue;
        }
        let character = line[index..]
            .chars()
            .next()
            .unwrap_or(char::REPLACEMENT_CHARACTER);
        output.push(character);
        index += character.len_utf8();
        column += 1;
    }
    if highlighted {
        output.push_str("\x1b[27m");
    }
    output
}

pub(super) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(char::from(ALPHABET[usize::from(first >> 2)]));
        encoded.push(char::from(
            ALPHABET[usize::from((first & 0x03) << 4 | second >> 4)],
        ));
        encoded.push(if chunk.len() > 1 {
            char::from(ALPHABET[usize::from((second & 0x0f) << 2 | third >> 6)])
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            char::from(ALPHABET[usize::from(third & 0x3f)])
        } else {
            '='
        });
    }
    encoded
}

pub(super) fn copy_command(program: &str, arguments: &[&str], text: &str) -> bool {
    let Ok(mut child) = std::process::Command::new(program)
        .args(arguments)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    let succeeded = child.wait().is_ok_and(|status| status.success());
    wrote && succeeded
}

#[cfg(target_os = "macos")]
pub(super) fn copy_with_platform_command(text: &str) -> bool {
    copy_command("pbcopy", &[], text)
}

#[cfg(target_os = "linux")]
pub(super) fn copy_with_platform_command(text: &str) -> bool {
    copy_command("wl-copy", &[], text)
        || copy_command("xclip", &["-selection", "clipboard"], text)
        || copy_command("xsel", &["--clipboard", "--input"], text)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn copy_with_platform_command(_text: &str) -> bool {
    false
}

pub(super) fn terminal_size() -> (u16, u16) {
    // SAFETY: A zeroed winsize has a valid all-integer representation and is
    // passed to ioctl as writable storage.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: STDOUT_FILENO is a terminal while the TUI is active and `size`
    // points to writable winsize storage.
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
        && size.ws_col > 0
        && size.ws_row > 0
    {
        (size.ws_col.max(20), size.ws_row.max(8))
    } else {
        (80, 24)
    }
}
