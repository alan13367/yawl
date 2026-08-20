//! Terminal input decoding for keys, bracketed paste, kitty CSI-u events,
//! and SGR mouse-wheel reports.

use std::collections::VecDeque;
use std::io::{self, Read};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    /// Shift+Enter or Alt+Enter.
    Newline,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    Escape,
    Ctrl(char),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Key(Key),
    Paste(String),
    /// Positive values scroll toward older content.
    MouseScroll(i32),
    /// Raw mode uses a short read timeout, which also lets the UI notice
    /// terminal resizes and signal-handler state changes.
    Tick,
}

pub struct EventReader<R> {
    input: R,
    pending: VecDeque<u8>,
}

impl<R: Read> EventReader<R> {
    pub fn new(input: R) -> Self {
        Self {
            input,
            pending: VecDeque::new(),
        }
    }

    pub fn read_event(&mut self) -> io::Result<Event> {
        let Some(byte) = self.read_byte()? else {
            return Ok(Event::Tick);
        };
        match byte {
            b'\r' | b'\n' => Ok(Event::Key(Key::Enter)),
            b'\t' => Ok(Event::Key(Key::Tab)),
            0x7f | 0x08 => Ok(Event::Key(Key::Backspace)),
            0x1b => self.read_escape(),
            0x01..=0x1a => Ok(Event::Key(Key::Ctrl(char::from(b'a' + byte - 1)))),
            0x20..=0x7e => Ok(Event::Key(Key::Char(char::from(byte)))),
            _ => Ok(Event::Key(Key::Char(self.read_utf8(byte)?))),
        }
    }

    fn read_escape(&mut self) -> io::Result<Event> {
        let Some(next) = self.read_byte()? else {
            return Ok(Event::Key(Key::Escape));
        };
        match next {
            b'[' => self.read_csi(),
            b'O' => {
                let key = match self.read_byte()? {
                    Some(b'A') => Key::Up,
                    Some(b'B') => Key::Down,
                    Some(b'C') => Key::Right,
                    Some(b'D') => Key::Left,
                    Some(b'H') => Key::Home,
                    Some(b'F') => Key::End,
                    Some(other) => {
                        self.pending.push_front(other);
                        Key::Escape
                    }
                    None => Key::Escape,
                };
                Ok(Event::Key(key))
            }
            b'\r' | b'\n' => Ok(Event::Key(Key::Newline)),
            other => {
                // Alt-modified printable keys are inserted as their base
                // character. Alt+Enter is handled above as the multiline
                // fallback.
                if other.is_ascii() {
                    Ok(Event::Key(Key::Char(char::from(other))))
                } else {
                    Ok(Event::Key(Key::Char(self.read_utf8(other)?)))
                }
            }
        }
    }

    fn read_csi(&mut self) -> io::Result<Event> {
        let mut body = Vec::new();
        let final_byte = loop {
            let Some(byte) = self.read_byte()? else {
                return Ok(Event::Key(Key::Escape));
            };
            if (0x40..=0x7e).contains(&byte) {
                break byte;
            }
            body.push(byte);
            if body.len() > 64 {
                return Ok(Event::Key(Key::Escape));
            }
        };
        let body = String::from_utf8_lossy(&body);
        if final_byte == b'~' && body == "200" {
            return self.read_paste();
        }
        if body.starts_with('<') && matches!(final_byte, b'M' | b'm') {
            return Ok(parse_mouse(&body).unwrap_or(Event::Tick));
        }
        if final_byte == b'u' {
            return Ok(Event::Key(parse_kitty_key(&body).unwrap_or(Key::Escape)));
        }
        let key = match (body.as_ref(), final_byte) {
            (_, b'A') => Key::Up,
            (_, b'B') => Key::Down,
            (_, b'C') => Key::Right,
            (_, b'D') => Key::Left,
            (_, b'H') | ("1", b'~') => Key::Home,
            (_, b'F') | ("4", b'~') => Key::End,
            ("3", b'~') => Key::Delete,
            ("5", b'~') => Key::PageUp,
            ("6", b'~') => Key::PageDown,
            _ => Key::Escape,
        };
        Ok(Event::Key(key))
    }

    fn read_paste(&mut self) -> io::Result<Event> {
        const END: &[u8] = b"\x1b[201~";
        let mut bytes = Vec::new();
        let mut idle_reads = 0u8;
        loop {
            let Some(byte) = self.read_byte()? else {
                idle_reads = idle_reads.saturating_add(1);
                if idle_reads >= 20 {
                    break;
                }
                continue;
            };
            idle_reads = 0;
            bytes.push(byte);
            if bytes.ends_with(END) {
                bytes.truncate(bytes.len() - END.len());
                break;
            }
        }
        let text = String::from_utf8_lossy(&bytes)
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        Ok(Event::Paste(text))
    }

    fn read_utf8(&mut self, first: u8) -> io::Result<char> {
        let width = utf8_width(first);
        if width == 0 {
            return Ok(char::REPLACEMENT_CHARACTER);
        }
        let mut bytes = vec![first];
        for _ in 1..width {
            let Some(byte) = self.read_byte()? else {
                return Ok(char::REPLACEMENT_CHARACTER);
            };
            bytes.push(byte);
        }
        Ok(std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| text.chars().next())
            .unwrap_or(char::REPLACEMENT_CHARACTER))
    }

    fn read_byte(&mut self) -> io::Result<Option<u8>> {
        if let Some(byte) = self.pending.pop_front() {
            return Ok(Some(byte));
        }
        let mut byte = [0u8; 1];
        loop {
            match self.input.read(&mut byte) {
                Ok(0) => return Ok(None),
                Ok(_) => return Ok(Some(byte[0])),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

fn utf8_width(first: u8) -> usize {
    match first {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    }
}

fn parse_kitty_key(body: &str) -> Option<Key> {
    let mut fields = body.split(';');
    let code = fields.next()?.split(':').next()?.parse::<u32>().ok()?;
    let modifier = fields
        .next()
        .and_then(|field| field.split(':').next())
        .and_then(|field| field.parse::<u8>().ok())
        .unwrap_or(1);
    let shift = modifier.saturating_sub(1) & 1 != 0;
    let alt = modifier.saturating_sub(1) & 2 != 0;
    let ctrl = modifier.saturating_sub(1) & 4 != 0;
    if code == 13 {
        return Some(if shift || alt {
            Key::Newline
        } else {
            Key::Enter
        });
    }
    if ctrl
        && let Some(character) = char::from_u32(code)
        && character.is_ascii_alphabetic()
    {
        return Some(Key::Ctrl(character.to_ascii_lowercase()));
    }
    char::from_u32(code).map(Key::Char)
}

fn parse_mouse(body: &str) -> Option<Event> {
    let button = body
        .strip_prefix('<')?
        .split(';')
        .next()?
        .parse::<u16>()
        .ok()?;
    // Ignore Shift/Alt/Ctrl modifier bits while preserving the wheel code.
    match button & !0b1_1100 {
        64 => Some(Event::MouseScroll(3)),
        65 => Some(Event::MouseScroll(-3)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn decodes_arrows_and_alt_enter() {
        let mut reader = EventReader::new(Cursor::new(b"\x1b[A\x1b\r"));
        assert_eq!(reader.read_event().unwrap(), Event::Key(Key::Up));
        assert_eq!(reader.read_event().unwrap(), Event::Key(Key::Newline));
    }

    #[test]
    fn decodes_kitty_shift_enter() {
        let mut reader = EventReader::new(Cursor::new(b"\x1b[13;2u"));
        assert_eq!(reader.read_event().unwrap(), Event::Key(Key::Newline));
    }

    #[test]
    fn collects_bracketed_paste() {
        let input = b"\x1b[200~one\r\ntwo\x1b[201~";
        let mut reader = EventReader::new(Cursor::new(input));
        assert_eq!(
            reader.read_event().unwrap(),
            Event::Paste("one\ntwo".into())
        );
    }

    #[test]
    fn decodes_mouse_wheel() {
        let mut reader = EventReader::new(Cursor::new(b"\x1b[<64;10;4M\x1b[<65;10;4M"));
        assert_eq!(reader.read_event().unwrap(), Event::MouseScroll(3));
        assert_eq!(reader.read_event().unwrap(), Event::MouseScroll(-3));
    }
}
