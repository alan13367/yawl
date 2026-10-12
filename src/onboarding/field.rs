//! Inline text fields for the setup wizard: provider names, URLs, API keys,
//! environment variables, and model IDs.
//!
//! A field draws two lines in place: the label with an editable value, and a
//! line for help or the latest validation error. Defaults are prefilled and
//! editable, keys are masked with a Tab toggle, and Esc goes back. Problems
//! replace the help line instead of scrolling new prompts.

use std::io::{self, Read, Write};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::error::Error;

const ERROR_COLOR: &str = "\x1b[38;5;210m";

/// What to ask for.
pub(super) struct Field<'a> {
    pub(super) label: &'a str,
    /// Prefilled, editable value.
    pub(super) initial: &'a str,
    /// Dim example shown while the value is empty.
    pub(super) placeholder: &'a str,
    pub(super) help: &'a str,
    pub(super) secret: bool,
}

/// Edits `field` in raw mode until the value passes `validate` (`Some`) or
/// the user presses Esc (`None`).
pub(super) fn ask(
    field: &Field<'_>,
    validate: &dyn Fn(&str) -> Result<(), String>,
) -> Result<Option<String>, Error> {
    let mut out = io::stdout();
    let _raw = crate::terminal_mode::RawMode::enter()?;
    let mut state = State::new(field.initial);
    draw(&mut out, field, &state, true)?;
    loop {
        match state.apply(read_key(&mut read_byte)?) {
            Outcome::Continue => {}
            Outcome::Cancel => {
                write!(out, "\r\x1b[2K\r\n\x1b[2K\x1b[1A\r")?;
                out.flush()?;
                return Ok(None);
            }
            Outcome::Submit => {
                let value = state.value().trim().to_string();
                match validate(&value) {
                    Ok(()) => {
                        let shown = if field.secret {
                            format!("{} characters, hidden", value.chars().count())
                        } else {
                            value.clone()
                        };
                        write!(out, "\r\x1b[2K{}: {shown}\r\n\x1b[2K", field.label)?;
                        out.flush()?;
                        return Ok(Some(value));
                    }
                    Err(error) => state.error = Some(error),
                }
            }
        }
        draw(&mut out, field, &state, false)?;
    }
}

/// Redraws both lines and leaves the cursor at the edit position.
fn draw(out: &mut impl Write, field: &Field<'_>, state: &State, first: bool) -> Result<(), Error> {
    let width = crate::terminal_mode::terminal_columns().max(20);
    let (line, cursor) = render_line(field, state, width);
    let message = render_message(field, state, width);
    // Raw mode needs explicit \r\n; the message line sits below the value.
    if first {
        write!(out, "\r\n\x1b[1A")?;
    }
    write!(
        out,
        "\r\x1b[2K{line}\r\n\x1b[2K{message}\x1b[1A\r\x1b[{}C",
        cursor.max(1)
    )?;
    if cursor == 0 {
        write!(out, "\r")?;
    }
    out.flush()?;
    Ok(())
}

/// The label and the part of the value that fits, plus the cursor column.
/// Widths are display cells, so wide characters keep the cursor in place.
fn render_line(field: &Field<'_>, state: &State, width: usize) -> (String, usize) {
    let prefix_width = format!("{}: ", field.label).width();
    let room = width.saturating_sub(prefix_width + 1).max(4);
    let cells = |chars: &[char]| -> usize {
        chars
            .iter()
            .map(|character| character.width().unwrap_or(0))
            .sum()
    };
    if state.chars.is_empty() {
        let placeholder = fit(field.placeholder.chars(), room);
        return (
            format!("\x1b[1m{}\x1b[0m: \x1b[2m{placeholder}\x1b[0m", field.label),
            prefix_width,
        );
    }
    let shown: Vec<char> = if field.secret && !state.revealed {
        vec!['•'; state.chars.len()]
    } else {
        state.chars.clone()
    };
    // Scroll so the cursor stays on screen; one cell is kept for it.
    let cursor = state.cursor.min(shown.len());
    let mut start = 0;
    while start < cursor && cells(&shown[start..cursor]) >= room {
        start += 1;
    }
    let visible = fit(shown[start..].iter().copied(), room);
    (
        format!("\x1b[1m{}\x1b[0m: {visible}", field.label),
        prefix_width + cells(&shown[start..cursor]),
    )
}

/// The leading characters that fit in `room` display cells.
fn fit(characters: impl Iterator<Item = char>, room: usize) -> String {
    let mut text = String::new();
    let mut used = 0;
    for character in characters {
        let cell = character.width().unwrap_or(0);
        if used + cell > room {
            break;
        }
        text.push(character);
        used += cell;
    }
    text
}

fn render_message(field: &Field<'_>, state: &State, width: usize) -> String {
    let keys = if field.secret {
        "Enter continue · Tab show/hide · Esc back"
    } else {
        "Enter continue · Esc back"
    };
    let (text, color) = match &state.error {
        Some(error) => (format!("✗ {error}"), ERROR_COLOR),
        None if field.help.is_empty() => (keys.to_string(), "\x1b[2m"),
        None => (format!("{} · {keys}", field.help), "\x1b[2m"),
    };
    let text: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{color}{text}\x1b[0m")
}

#[derive(Debug, PartialEq, Eq)]
enum Key {
    Char(char),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    ClearBefore,
    ClearAfter,
    DeleteWord,
    Tab,
    Enter,
    Escape,
    Other,
}

enum Outcome {
    Continue,
    Submit,
    Cancel,
}

struct State {
    chars: Vec<char>,
    cursor: usize,
    revealed: bool,
    error: Option<String>,
}

impl State {
    fn new(initial: &str) -> Self {
        let chars: Vec<char> = initial.chars().collect();
        Self {
            cursor: chars.len(),
            chars,
            revealed: false,
            error: None,
        }
    }

    fn value(&self) -> String {
        self.chars.iter().collect()
    }

    fn apply(&mut self, key: Key) -> Outcome {
        let edited = match key {
            Key::Enter => return Outcome::Submit,
            Key::Escape => return Outcome::Cancel,
            Key::Tab => {
                self.revealed = !self.revealed;
                false
            }
            Key::Char(character) => {
                self.chars.insert(self.cursor, character);
                self.cursor += 1;
                true
            }
            Key::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.chars.remove(self.cursor);
                true
            }
            Key::Delete if self.cursor < self.chars.len() => {
                self.chars.remove(self.cursor);
                true
            }
            Key::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                false
            }
            Key::Right => {
                self.cursor = (self.cursor + 1).min(self.chars.len());
                false
            }
            Key::Home => {
                self.cursor = 0;
                false
            }
            Key::End => {
                self.cursor = self.chars.len();
                false
            }
            Key::ClearBefore => {
                self.chars.drain(..self.cursor);
                self.cursor = 0;
                true
            }
            Key::ClearAfter => {
                self.chars.truncate(self.cursor);
                true
            }
            Key::DeleteWord => {
                let end = self.cursor;
                while self.cursor > 0 && self.chars[self.cursor - 1].is_whitespace() {
                    self.cursor -= 1;
                }
                while self.cursor > 0 && !self.chars[self.cursor - 1].is_whitespace() {
                    self.cursor -= 1;
                }
                self.chars.drain(self.cursor..end);
                true
            }
            Key::Backspace | Key::Delete | Key::Other => false,
        };
        if edited {
            self.error = None;
        }
        Outcome::Continue
    }
}

/// Reads one key, waiting through read timeouts. Ctrl+C still raises an
/// interrupt because raw mode keeps signals on.
fn read_key(read: &mut impl FnMut() -> io::Result<Option<u8>>) -> Result<Key, Error> {
    loop {
        match read() {
            Ok(Some(byte)) => return decode(byte, read),
            Ok(None) => continue,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                if crate::interrupted() {
                    return Err(Error::Interrupted);
                }
            }
            Err(error) => return Err(Error::Io(error)),
        }
    }
}

fn decode(byte: u8, read: &mut impl FnMut() -> io::Result<Option<u8>>) -> Result<Key, Error> {
    let mut next = || match read() {
        Ok(byte) => Ok(byte),
        Err(error) if error.kind() == io::ErrorKind::Interrupted && crate::interrupted() => {
            Err(Error::Interrupted)
        }
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(None),
        Err(error) => Err(Error::Io(error)),
    };
    Ok(match byte {
        b'\r' | b'\n' => Key::Enter,
        b'\t' => Key::Tab,
        0x7f | 0x08 => Key::Backspace,
        0x01 => Key::Home,
        0x05 => Key::End,
        0x15 => Key::ClearBefore,
        0x0b => Key::ClearAfter,
        0x17 => Key::DeleteWord,
        0x1b => match next()? {
            // A lone Esc goes back; nothing followed within the read timeout.
            None | Some(0x1b) => Key::Escape,
            Some(b'[' | b'O') => match next()? {
                Some(b'C') => Key::Right,
                Some(b'D') => Key::Left,
                Some(b'H') => Key::Home,
                Some(b'F') => Key::End,
                Some(digit @ b'0'..=b'9') => {
                    // `ESC [ n ~` sequences: 1/7 Home, 4/8 End, 3 Delete.
                    let mut code = vec![digit];
                    while let Some(byte) = next()? {
                        if byte == b'~' {
                            break;
                        }
                        code.push(byte);
                    }
                    match code.as_slice() {
                        b"1" | b"7" => Key::Home,
                        b"4" | b"8" => Key::End,
                        b"3" => Key::Delete,
                        _ => Key::Other,
                    }
                }
                _ => Key::Other,
            },
            Some(_) => Key::Other,
        },
        byte if byte < 0x20 => Key::Other,
        byte => {
            // Gather the rest of a UTF-8 sequence from its lead byte.
            let extra = match byte {
                0xc0..=0xdf => 1,
                0xe0..=0xef => 2,
                0xf0..=0xf7 => 3,
                _ => 0,
            };
            let mut bytes = vec![byte];
            for _ in 0..extra {
                match next()? {
                    Some(byte) => bytes.push(byte),
                    None => break,
                }
            }
            match std::str::from_utf8(&bytes)
                .ok()
                .and_then(|text| text.chars().next())
            {
                Some(character) => Key::Char(character),
                None => Key::Other,
            }
        }
    })
}

fn read_byte() -> io::Result<Option<u8>> {
    let mut buffer = [0u8; 1];
    Ok((io::stdin().read(&mut buffer)? == 1).then_some(buffer[0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(bytes: &[u8]) -> Vec<Key> {
        let mut input = bytes.iter().copied();
        let mut read = || Ok(input.next());
        let mut keys = Vec::new();
        while let Some(byte) = read().ok().flatten() {
            keys.push(decode(byte, &mut read).expect("decodes"));
        }
        keys
    }

    fn field(secret: bool) -> Field<'static> {
        Field {
            label: "API key",
            initial: "",
            placeholder: "Paste your key",
            help: "Saved in config.json",
            secret,
        }
    }

    #[test]
    fn decodes_editing_keys_arrows_and_utf8() {
        assert_eq!(
            keys(b"a\x7f\x1b[D\x1b[C\x1b[3~\x1b[H\x1b[4~\x15\x17\t\r"),
            [
                Key::Char('a'),
                Key::Backspace,
                Key::Left,
                Key::Right,
                Key::Delete,
                Key::Home,
                Key::End,
                Key::ClearBefore,
                Key::DeleteWord,
                Key::Tab,
                Key::Enter,
            ]
        );
        assert_eq!(keys("é".as_bytes()), [Key::Char('é')]);
        assert_eq!(keys(b"\x1b"), [Key::Escape]);
    }

    #[test]
    fn defaults_are_editable_in_place() {
        let mut state = State::new("http://localhost:11434/v1");
        for key in [Key::Home, Key::Right, Key::Right, Key::Right, Key::Right] {
            state.apply(key);
        }
        state.apply(Key::Char('s'));
        assert_eq!(state.value(), "https://localhost:11434/v1");
        state.apply(Key::End);
        state.apply(Key::DeleteWord);
        assert_eq!(state.value(), "");
        state.apply(Key::Char('x'));
        assert!(matches!(state.apply(Key::Enter), Outcome::Submit));
        assert!(matches!(state.apply(Key::Escape), Outcome::Cancel));
    }

    #[test]
    fn secrets_mask_until_tab_and_errors_clear_on_edit() {
        let field = field(true);
        let mut state = State::new("");
        let (line, cursor) = render_line(&field, &state, 80);
        assert!(line.contains("Paste your key"));
        assert_eq!(cursor, "API key: ".len());

        for character in "sk-123".chars() {
            state.apply(Key::Char(character));
        }
        let (line, _) = render_line(&field, &state, 80);
        assert!(line.contains("••••••") && !line.contains("sk-123"));
        state.apply(Key::Tab);
        assert!(render_line(&field, &state, 80).0.contains("sk-123"));

        state.error = Some("The API key must not be empty".into());
        assert!(render_message(&field, &state, 80).contains("✗ The API key"));
        state.apply(Key::Char('4'));
        let message = render_message(&field, &state, 80);
        assert!(message.contains("Saved in config.json") && message.contains("Tab show/hide"));
    }

    #[test]
    fn long_values_scroll_with_the_cursor() {
        let field = Field {
            secret: false,
            ..field(false)
        };
        let state = State::new(&"x".repeat(200));
        let (_, cursor) = render_line(&field, &state, 40);
        assert!(cursor < 40);
    }

    #[test]
    fn wide_characters_place_the_cursor_by_display_width() {
        let field = field(false);
        let mut state = State::new("模型");
        let (_, cursor) = render_line(&field, &state, 80);
        assert_eq!(cursor, field.label.len() + 2 + 4);

        state = State::new(&"模".repeat(100));
        let (line, cursor) = render_line(&field, &state, 40);
        assert!(cursor < 40, "{cursor}");
        let value = line.rsplit_once(": ").map_or("", |(_, value)| value);
        assert!(field.label.len() + 2 + value.width() < 40, "{line}");
    }
}
