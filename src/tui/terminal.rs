//! Raw terminal lifecycle, frame output, selection, and clipboard support.

use std::io::{self, IsTerminal, Write};

use base64::Engine as _;

use crate::error::Error;
use crate::terminal_mode::RawMode;

use super::events::{MouseEvent, MouseKind};
use super::input::Editor;
use super::remote::{self, Remote, Snapshot};
use super::render::{FrameImage, HIDDEN_CURSOR, ImageSupport, build_frame_with_images};
use super::{ViewState, markdown};

pub(super) struct Terminal {
    raw_mode: RawMode,
    stdout: io::Stdout,
    active: bool,
    focused: bool,
    last_frame: Vec<String>,
    last_base_frame: Vec<String>,
    last_images: Vec<DisplayedImage>,
    last_size: (u16, u16),
    selection: Option<TextSelection>,
    image_protocol: ImageProtocol,
    mouse_mode: MouseMode,
    remote: Remote,
    /// Remote state as of the last draw.
    remote_controlled: bool,
    remote_revision: u64,
    /// Whether host Ctrl+C raises SIGINT; off while a remote session runs.
    host_signals: bool,
    /// Host lock screen as of the last draw while a device has control.
    lock_frame: Vec<String>,
    lock_size: (u16, u16),
}

/// All-motion reports are needed only while the Git dashboard is visible.
pub(super) struct MouseMode {
    tracking: bool,
    pointing: bool,
    pointer_shapes: bool,
}

impl MouseMode {
    pub(super) fn new(pointer_shapes: bool) -> Self {
        Self {
            tracking: false,
            pointing: false,
            pointer_shapes,
        }
    }

    pub(super) fn update(
        &mut self,
        output: &mut impl Write,
        tracking: bool,
        pointing: bool,
    ) -> io::Result<()> {
        if self.tracking != tracking {
            output.write_all(if tracking {
                b"\x1b[?1002l\x1b[?1003h"
            } else {
                b"\x1b[?1003l\x1b[?1002h"
            })?;
            self.tracking = tracking;
        }
        let pointing = tracking && pointing && self.pointer_shapes;
        if self.pointing != pointing {
            output.write_all(if pointing {
                b"\x1b]22;pointer\x1b\\"
            } else {
                b"\x1b]22;\x1b\\"
            })?;
            self.pointing = pointing;
        }
        Ok(())
    }
}

pub(super) fn supports_pointer_shapes(
    term_program: Option<&str>,
    term: Option<&str>,
    kitty_window: bool,
) -> bool {
    kitty_window
        || term_program.is_some_and(|name| {
            name.eq_ignore_ascii_case("ghostty") || name.eq_ignore_ascii_case("kitty")
        })
        || term.is_some_and(|name| {
            name == "xterm-kitty"
                || name == "xterm-ghostty"
                || name == "foot"
                || name == "foot-extra"
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ImageProtocol {
    None,
    Kitty,
    Iterm2,
}

impl ImageProtocol {
    fn detect() -> Self {
        Self::from_terminal(
            std::env::var("TERM_PROGRAM").ok().as_deref(),
            std::env::var_os("KITTY_WINDOW_ID").is_some(),
            std::env::var("TERM").ok().as_deref(),
        )
    }

    pub(super) fn from_terminal(
        term_program: Option<&str>,
        kitty_window: bool,
        term: Option<&str>,
    ) -> Self {
        if kitty_window
            || term.is_some_and(|value| value.contains("kitty"))
            || term_program.is_some_and(|value| {
                value.eq_ignore_ascii_case("ghostty") || value.eq_ignore_ascii_case("wezterm")
            })
        {
            Self::Kitty
        } else if term_program.is_some_and(|value| value.eq_ignore_ascii_case("iterm.app")) {
            Self::Iterm2
        } else {
            Self::None
        }
    }

    fn support(self) -> ImageSupport {
        match self {
            Self::None => ImageSupport::None,
            Self::Kitty => ImageSupport::Png,
            Self::Iterm2 => ImageSupport::All,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisplayedImage {
    key: usize,
    row: usize,
    column: usize,
    columns: usize,
    rows: usize,
}

impl From<&FrameImage> for DisplayedImage {
    fn from(image: &FrameImage) -> Self {
        Self {
            key: image.key,
            row: image.row,
            column: image.column,
            columns: image.columns,
            rows: image.rows,
        }
    }
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
        let raw_mode = RawMode::enter()?;

        let mut terminal = Self {
            raw_mode,
            stdout: io::stdout(),
            active: true,
            focused: true,
            last_frame: Vec::new(),
            last_base_frame: Vec::new(),
            last_images: Vec::new(),
            last_size: (0, 0),
            selection: None,
            image_protocol: ImageProtocol::detect(),
            mouse_mode: MouseMode::new(supports_pointer_shapes(
                std::env::var("TERM_PROGRAM").ok().as_deref(),
                std::env::var("TERM").ok().as_deref(),
                std::env::var_os("KITTY_WINDOW_ID").is_some(),
            )),
            remote: Remote::new(),
            remote_controlled: false,
            remote_revision: 0,
            host_signals: true,
            lock_frame: Vec::new(),
            lock_size: (0, 0),
        };
        terminal.stdout.write_all(
            b"\x1b[?1049h\x1b[2J\x1b[H\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[?2004h\x1b[>1u\x1b[=1;1u\x1b[>4;1m",
        )?;
        terminal.stdout.flush()?;
        Ok(terminal)
    }

    pub(super) fn invalidate(&mut self) {
        self.last_frame.clear();
        self.last_base_frame.clear();
        self.last_size = (0, 0);
    }

    /// Reports whether the next draw would differ because of size or remote
    /// session changes, even without new UI state.
    pub(super) fn size_changed(&self) -> bool {
        let remote = self.remote.snapshot();
        if remote.controlled != self.remote_controlled || remote.revision != self.remote_revision {
            return true;
        }
        if remote.controlled {
            terminal_size() != self.lock_size
        } else {
            terminal_size() != self.last_size
        }
    }

    pub(super) fn remote(&self) -> &Remote {
        &self.remote
    }

    /// Host Ctrl+C must reach the input filter as a byte instead of raising
    /// SIGINT and interrupting whatever the remote device started. Signals
    /// stay off for the whole session, from before any device can attach;
    /// the UI reads Ctrl+C as a key meanwhile, as on kitty-protocol
    /// terminals.
    pub(super) fn sync_host_signals(&mut self, session_running: bool) -> Result<(), Error> {
        let enabled = !session_running;
        if enabled != self.host_signals {
            self.raw_mode.set_signals(enabled)?;
            self.host_signals = enabled;
        }
        Ok(())
    }

    /// Applies remote-session transitions before a draw. Returns the frame
    /// size and whether the frame goes to the remote device.
    fn sync_remote(&mut self, remote: &Snapshot) -> Result<((u16, u16), bool), Error> {
        // Restoring signals after a session is best effort: until it
        // succeeds, Ctrl+C still arrives as a key.
        let _ = self.sync_host_signals(remote.address.is_some());
        if remote.controlled != self.remote_controlled {
            self.remote_controlled = remote.controlled;
            self.selection = None;
            self.lock_frame.clear();
            self.lock_size = (0, 0);
            if remote.controlled {
                self.mouse_mode.update(&mut self.stdout, false, false)?;
            }
            self.invalidate();
        }
        if remote.revision != self.remote_revision {
            self.remote_revision = remote.revision;
            // A newly attached device has no previous frame to diff against.
            self.invalidate();
        }
        Ok(if remote.controlled {
            (remote.size.unwrap_or(remote::DEFAULT_SIZE), true)
        } else {
            (terminal_size(), false)
        })
    }

    fn draw_lock_screen(&mut self, remote: &Snapshot) -> Result<(), Error> {
        let (columns, rows) = terminal_size();
        let screen = remote::lock_screen(remote, columns, rows);
        if (columns, rows) == self.lock_size && screen == self.lock_frame {
            return Ok(());
        }
        self.stdout.write_all(b"\x1b[?25l")?;
        if self.image_protocol == ImageProtocol::Kitty {
            self.stdout.write_all(b"\x1b_Ga=d,d=A,q=2;\x1b\\")?;
        }
        self.stdout.write_all(b"\x1b[2J")?;
        write_frame_rows(&mut self.stdout, &screen, &[], true)?;
        self.stdout.flush()?;
        self.lock_frame = screen;
        self.lock_size = (columns, rows);
        Ok(())
    }

    pub(super) fn handle_mouse(&mut self, event: MouseEvent) -> Result<bool, Error> {
        let point = ScreenPoint {
            row: event.row,
            column: event.column,
        };
        match event.kind {
            MouseKind::Move => Ok(false),
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
                self.copy_text(&text)?;
                Ok(true)
            }
        }
    }

    pub(super) fn draw(&mut self, state: &mut ViewState, editor: &Editor) -> Result<(), Error> {
        let remote = self.remote.snapshot();
        if remote.address.is_some() {
            self.remote.update_title(state);
        }
        let ((columns, rows), to_remote) = self.sync_remote(&remote)?;
        if self.last_size != (columns, rows) {
            super::git::clear_hover(state);
        }
        // Browsers get no inline images; only the host terminal protocol is
        // known.
        let image_protocol = if to_remote {
            ImageProtocol::None
        } else {
            self.image_protocol
        };
        let rendered = build_frame_with_images(
            state,
            editor,
            usize::from(columns),
            usize::from(rows),
            image_protocol.support(),
        );
        let base_frame = rendered.lines;
        let cursor = rendered.cursor;
        let images = rendered.images;
        self.last_base_frame.clone_from(&base_frame);
        let frame = self
            .selection
            .as_ref()
            .map_or(base_frame, highlighted_selection);
        let displayed_images = images.iter().map(DisplayedImage::from).collect::<Vec<_>>();
        let images_changed = self.last_images != displayed_images;
        let image_rows_changed = images.iter().any(|image| {
            let start = image.row.saturating_sub(1);
            let end = start.saturating_add(image.rows).min(frame.len());
            (start..end).any(|row| self.last_frame.get(row) != frame.get(row))
        });
        let force = self.last_size != (columns, rows)
            || self.last_frame.len() != frame.len()
            || images_changed
            || image_rows_changed;
        let mut output = Vec::new();
        output.extend_from_slice(b"\x1b[?25l");
        if force {
            if image_protocol == ImageProtocol::Kitty && !self.last_images.is_empty() {
                output.extend_from_slice(b"\x1b_Ga=d,d=A,q=2;\x1b\\");
            }
            output.extend_from_slice(b"\x1b[2J");
        }
        write_frame_rows(&mut output, &frame, &self.last_frame, force)?;
        if force {
            write_inline_images(&mut output, image_protocol, &images)?;
        }
        output.extend_from_slice(cursor_control(cursor, self.selection.is_some()).as_bytes());
        if to_remote {
            self.remote
                .send_frame(String::from_utf8_lossy(&output).into_owned());
            self.draw_lock_screen(&remote)?;
        } else {
            self.stdout.write_all(&output)?;
            let git_visible = state.git_view.is_some()
                && state.git_init.is_none()
                && state.process_view.is_none();
            // Any-event tracking reports motion without a held button. The
            // Git dashboard uses it for row hover; the scroll-to-bottom pill
            // uses it while that pill is on screen. Clicks and drags stay on
            // button-event tracking the rest of the time.
            let jump_visible = state.follow.button.is_some();
            self.mouse_mode.update(
                &mut self.stdout,
                git_visible || jump_visible,
                self.focused
                    && (super::git::pointer_over_control(state)
                        || super::state::pointer_over_jump_button(state)),
            )?;
            self.stdout.flush()?;
        }
        self.last_frame = frame;
        self.last_images = displayed_images;
        self.last_size = (columns, rows);
        Ok(())
    }

    /// Copies `text` via a platform clipboard command, falling back to OSC 52.
    /// While a remote device has control, the text goes to that device.
    ///
    /// # Errors
    ///
    /// Returns I/O errors from writing the OSC 52 sequence.
    pub(super) fn copy_text(&mut self, text: &str) -> Result<bool, Error> {
        if text.is_empty() {
            return Ok(false);
        }
        if self.remote_controlled {
            return Ok(self.remote.send_clipboard(text));
        }
        if !copy_with_platform_command(text) {
            let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
            write!(self.stdout, "\x1b]52;c;{encoded}\x07")?;
            self.stdout.flush()?;
        }
        Ok(true)
    }

    pub(super) fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Rings the terminal bell when the terminal is not focused so an unfocused
    /// terminal (macOS Terminal tab badge, audible bell) can announce a settled
    /// turn or a pending question. Best effort: failures never interrupt the
    /// event loop.
    pub(super) fn ring_bell(&mut self) {
        if self.remote_controlled {
            self.remote.send_bell();
            return;
        }
        Self::ring_bell_to(&mut self.stdout, self.focused);
    }

    pub(super) fn ring_bell_to(output: &mut impl Write, focused: bool) {
        if focused {
            return;
        }
        let _ = write!(output, "\x07");
        let _ = output.flush();
    }
}

pub(super) fn write_inline_images(
    output: &mut impl Write,
    protocol: ImageProtocol,
    images: &[FrameImage],
) -> io::Result<()> {
    for (index, image) in images.iter().enumerate() {
        write!(output, "\x1b[{};{}H", image.row, image.column)?;
        match protocol {
            ImageProtocol::None => {}
            ImageProtocol::Kitty => write_kitty_image(output, image, index + 1)?,
            ImageProtocol::Iterm2 => {
                write!(
                    output,
                    "\x1b]1337;File=inline=1;width={};height={};preserveAspectRatio=1;doNotMoveCursor=1:",
                    image.columns, image.rows
                )?;
                output.write_all(image.content.data.as_bytes())?;
                output.write_all(b"\x07")?;
            }
        }
    }
    Ok(())
}

fn write_kitty_image(
    output: &mut impl Write,
    image: &FrameImage,
    image_id: usize,
) -> io::Result<()> {
    let mut chunks = image.content.data.as_bytes().chunks(4096).peekable();
    let Some(first) = chunks.next() else {
        return Ok(());
    };
    let more = usize::from(chunks.peek().is_some());
    write!(
        output,
        "\x1b_Ga=T,f=100,t=d,q=2,C=1,i={image_id},c={},r={},m={more};",
        image.columns, image.rows
    )?;
    output.write_all(first)?;
    output.write_all(b"\x1b\\")?;
    while let Some(chunk) = chunks.next() {
        let more = usize::from(chunks.peek().is_some());
        write!(output, "\x1b_Gm={more};")?;
        output.write_all(chunk)?;
        output.write_all(b"\x1b\\")?;
    }
    Ok(())
}

pub(super) fn write_frame_rows(
    output: &mut impl Write,
    frame: &[String],
    previous: &[String],
    force: bool,
) -> io::Result<()> {
    // Every row is explicitly positioned. A terminal may measure Unicode
    // differently from us; never let an overlong row wrap into an unchanged
    // row below it, where incremental drawing would leave the damage behind.
    output.write_all(b"\x1b[?7l")?;
    let result = frame.iter().enumerate().try_for_each(|(index, line)| {
        if force || previous.get(index) != Some(line) {
            write!(output, "\x1b[{};1H\x1b[2K{line}", index + 1)?;
        }
        Ok(())
    });
    // Attempt restoration even if a row write failed.
    let restore = output.write_all(b"\x1b[?7h");
    result.and(restore)
}

pub(super) fn cursor_control(cursor: (usize, usize), selecting: bool) -> String {
    if selecting || cursor == HIDDEN_CURSOR {
        "\x1b[?25l".into()
    } else {
        format!("\x1b[{};{}H\x1b[?25h", cursor.0, cursor.1)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if self.image_protocol == ImageProtocol::Kitty && !self.last_images.is_empty() {
            let _ = self.stdout.write_all(b"\x1b_Ga=d,d=A,q=2;\x1b\\");
        }
        let _ = self.mouse_mode.update(&mut self.stdout, false, false);
        let _ = self.stdout.write_all(
            b"\x1b[>4;0m\x1b[=0;1u\x1b[<u\x1b[?1004l\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?25h\x1b[0m\x1b[?1049l",
        );
        let _ = self.stdout.flush();
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
