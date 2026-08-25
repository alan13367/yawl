//! Multiline input editor with cursor movement, kill keys, paste, and
//! command history.

use super::events::Key;

const LONG_PASTE_CHARS: usize = 400;
const LONG_PASTE_LINES: usize = 8;

#[derive(Debug, PartialEq, Eq)]
pub enum EditAction {
    None,
    Submit(String),
}

#[derive(Debug)]
pub struct InputLayout {
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

#[derive(Default)]
pub struct Editor {
    buffer: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: Vec<char>,
    pastes: Vec<String>,
}

impl Editor {
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn text(&self) -> String {
        self.buffer.iter().collect()
    }

    pub fn take_text(&mut self) -> Option<String> {
        let text = self.expand_pastes(&self.text());
        if text.trim().is_empty() {
            return None;
        }
        self.clear();
        Some(text)
    }

    /// Replaces long-paste placeholders with the original pasted text.
    pub fn expand_pastes(&self, text: &str) -> String {
        expand_paste_placeholders(text, &self.pastes)
    }

    /// Current slash-command token while the cursor is editing it.
    pub fn command_prefix(&self) -> Option<String> {
        if self.buffer.first() != Some(&'/') {
            return None;
        }
        let end = self
            .buffer
            .iter()
            .position(|character| character.is_whitespace())
            .unwrap_or(self.buffer.len());
        (self.cursor <= end).then(|| self.buffer[..end].iter().collect())
    }

    pub fn complete_command(&mut self, command: &str) {
        let end = self
            .buffer
            .iter()
            .position(|character| character.is_whitespace())
            .unwrap_or(self.buffer.len());
        self.buffer.splice(0..end, command.chars());
        self.cursor = command.chars().count();
        if self.buffer.get(self.cursor).is_none() {
            self.buffer.push(' ');
        }
        self.cursor += 1;
        self.leave_history();
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.history_index = None;
        self.history_draft.clear();
    }

    pub fn paste(&mut self, text: &str) {
        self.leave_history();
        let normalized = normalize_paste(text);
        if paste_is_long(&normalized) {
            let placeholder = paste_placeholder(self.pastes.len() + 1, &normalized);
            self.pastes.push(normalized);
            self.insert_chars(placeholder.chars());
            return;
        }
        self.insert_chars(normalized.chars());
    }

    pub fn handle_key(&mut self, key: Key) -> EditAction {
        match key {
            Key::Char(character) => self.insert(character),
            Key::Newline => self.insert('\n'),
            Key::Enter => return self.submit(),
            Key::Backspace => self.backspace(),
            Key::Delete => self.delete(),
            Key::Left => self.cursor = self.cursor.saturating_sub(1),
            Key::Right => self.cursor = (self.cursor + 1).min(self.buffer.len()),
            Key::Home | Key::Ctrl('a') => self.cursor = self.line_start(),
            Key::End | Key::Ctrl('e') => self.cursor = self.line_end(),
            Key::Up => self.history_previous(),
            Key::Down => self.history_next(),
            Key::Ctrl('u') => self.kill_to_line_start(),
            Key::Ctrl('k') => self.kill_to_line_end(),
            Key::Ctrl('w') => self.kill_previous_word(),
            Key::Tab => self.paste("    "),
            Key::PageUp | Key::PageDown | Key::Escape | Key::Ctrl(_) => {}
        }
        EditAction::None
    }

    pub fn layout(&self, width: usize) -> InputLayout {
        let width = width.max(3);
        let mut lines = Vec::new();
        let mut line = String::from("> ");
        let mut column = 2usize;
        let mut cursor_row = 0usize;
        let mut cursor_col = 2usize;

        for (index, character) in self.buffer.iter().copied().enumerate() {
            if index == self.cursor {
                cursor_row = lines.len();
                cursor_col = column;
            }
            if character == '\n' {
                lines.push(line);
                line = String::from("  ");
                column = 2;
                continue;
            }
            if column >= width {
                lines.push(line);
                line = String::from("  ");
                column = 2;
                if index == self.cursor {
                    cursor_row = lines.len();
                    cursor_col = column;
                }
            }
            line.push(if character.is_control() {
                '�'
            } else {
                character
            });
            column += 1;
        }
        if self.cursor == self.buffer.len() {
            cursor_row = lines.len();
            cursor_col = column;
        }
        lines.push(line);
        InputLayout {
            lines,
            cursor_row,
            cursor_col,
        }
    }

    fn insert(&mut self, character: char) {
        self.leave_history();
        self.buffer.insert(self.cursor, character);
        self.cursor += 1;
    }

    fn insert_chars(&mut self, chars: impl IntoIterator<Item = char>) {
        let chars: Vec<char> = chars.into_iter().collect();
        let count = chars.len();
        self.buffer.splice(self.cursor..self.cursor, chars);
        self.cursor += count;
    }

    fn backspace(&mut self) {
        self.leave_history();
        if self.cursor > 0 {
            self.cursor -= 1;
            self.buffer.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        self.leave_history();
        if self.cursor < self.buffer.len() {
            self.buffer.remove(self.cursor);
        }
    }

    fn line_start(&self) -> usize {
        self.buffer[..self.cursor]
            .iter()
            .rposition(|character| *character == '\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.buffer[self.cursor..]
            .iter()
            .position(|character| *character == '\n')
            .map_or(self.buffer.len(), |index| self.cursor + index)
    }

    fn kill_to_line_start(&mut self) {
        self.leave_history();
        let start = self.line_start();
        self.buffer.drain(start..self.cursor);
        self.cursor = start;
    }

    fn kill_to_line_end(&mut self) {
        self.leave_history();
        let end = self.line_end();
        self.buffer.drain(self.cursor..end);
    }

    fn kill_previous_word(&mut self) {
        self.leave_history();
        let mut start = self.cursor;
        while start > 0 && self.buffer[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !self.buffer[start - 1].is_whitespace() {
            start -= 1;
        }
        self.buffer.drain(start..self.cursor);
        self.cursor = start;
    }

    fn submit(&mut self) -> EditAction {
        let text: String = self.buffer.iter().collect();
        if text.trim().is_empty() {
            return EditAction::None;
        }
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        self.clear();
        EditAction::Submit(text)
    }

    fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_index {
            None => {
                self.history_draft.clone_from(&self.buffer);
                self.history.len() - 1
            }
            Some(index) => index.saturating_sub(1),
        };
        self.history_index = Some(next);
        self.buffer = self.history[next].chars().collect();
        self.cursor = self.buffer.len();
    }

    fn history_next(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.history.len() {
            let next = index + 1;
            self.history_index = Some(next);
            self.buffer = self.history[next].chars().collect();
        } else {
            self.history_index = None;
            self.buffer = std::mem::take(&mut self.history_draft);
        }
        self.cursor = self.buffer.len();
    }

    fn leave_history(&mut self) {
        self.history_index = None;
        self.history_draft.clear();
    }
}

fn normalize_paste(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut chars = String::with_capacity(normalized.len());
    for character in normalized.chars() {
        match character {
            '\t' => chars.push_str("    "),
            '\n' => chars.push('\n'),
            _ if !character.is_control() => chars.push(character),
            _ => {}
        }
    }
    chars
}

fn paste_is_long(text: &str) -> bool {
    text.chars().count() > LONG_PASTE_CHARS || text.split('\n').count() >= LONG_PASTE_LINES
}

fn paste_placeholder(id: usize, text: &str) -> String {
    format!("[Pasted #{id} {} characters]", text.chars().count())
}

fn expand_paste_placeholders(text: &str, pastes: &[String]) -> String {
    if pastes.is_empty() || !text.contains("[Pasted #") {
        return text.to_string();
    }
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(start) = rest.find("[Pasted #") else {
            result.push_str(rest);
            break;
        };
        result.push_str(&rest[..start]);
        let from_marker = &rest[start..];
        if let Some((placeholder_len, id)) = parse_paste_placeholder(from_marker) {
            if id > 0
                && let Some(paste) = pastes.get(id - 1)
            {
                result.push_str(paste);
            } else {
                result.push_str(&from_marker[..placeholder_len]);
            }
            rest = &from_marker[placeholder_len..];
        } else {
            result.push('[');
            rest = &from_marker[1..];
        }
    }
    result
}

fn parse_paste_placeholder(text: &str) -> Option<(usize, usize)> {
    let rest = text.strip_prefix("[Pasted #")?;
    let id_end = rest.find(' ')?;
    let id: usize = rest[..id_end].parse().ok()?;
    let after_id = &rest[id_end + 1..];
    let count_end = after_id.find(' ')?;
    let _count: usize = after_id[..count_end].parse().ok()?;
    let after_count = &after_id[count_end + 1..];
    let suffix = "characters]";
    if !after_count.starts_with(suffix) {
        return None;
    }
    let len = "[Pasted #".len() + id_end + 1 + count_end + 1 + suffix.len();
    Some((len, id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_enter_builds_multiline_submission() {
        let mut editor = Editor::default();
        editor.handle_key(Key::Char('a'));
        editor.handle_key(Key::Newline);
        editor.handle_key(Key::Char('b'));
        assert_eq!(
            editor.handle_key(Key::Enter),
            EditAction::Submit("a\nb".into())
        );
    }

    #[test]
    fn history_restores_draft() {
        let mut editor = Editor::default();
        editor.paste("first");
        editor.handle_key(Key::Enter);
        editor.paste("draft");
        editor.handle_key(Key::Up);
        assert_eq!(editor.layout(20).lines, ["> first"]);
        editor.handle_key(Key::Down);
        assert_eq!(editor.layout(20).lines, ["> draft"]);
    }

    #[test]
    fn completes_only_the_command_token() {
        let mut editor = Editor::default();
        editor.paste("/ski argument");
        editor.handle_key(Key::Home);
        assert_eq!(editor.command_prefix().as_deref(), Some("/ski"));
        editor.complete_command("/skill:review");
        assert_eq!(editor.layout(40).lines, ["> /skill:review argument"]);
    }

    #[test]
    fn layout_wraps_and_tracks_cursor() {
        let mut editor = Editor::default();
        editor.paste("abcdef");
        let layout = editor.layout(5);
        assert_eq!(layout.lines, ["> abc", "  def"]);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 5));
    }

    #[test]
    fn transient_text_does_not_enter_submission_history() {
        let mut editor = Editor::default();
        editor.paste("16384");

        assert_eq!(editor.take_text().as_deref(), Some("16384"));
        editor.handle_key(Key::Up);
        assert_eq!(editor.layout(20).lines, ["> "]);
    }

    #[test]
    fn long_paste_is_shown_as_a_placeholder_and_expanded_on_submit() {
        let mut editor = Editor::default();
        let pasted = "x".repeat(LONG_PASTE_CHARS + 1);
        editor.paste(&pasted);
        assert_eq!(editor.text(), "[Pasted #1 401 characters]");
        assert_eq!(editor.layout(40).lines, ["> [Pasted #1 401 characters]"]);
        assert_eq!(
            editor.handle_key(Key::Enter),
            EditAction::Submit("[Pasted #1 401 characters]".into())
        );
        assert_eq!(editor.expand_pastes("[Pasted #1 401 characters]"), pasted);
    }

    #[test]
    fn short_paste_stays_literal() {
        let mut editor = Editor::default();
        editor.paste("hello\nworld");
        assert_eq!(editor.text(), "hello\nworld");
        assert_eq!(
            editor.handle_key(Key::Enter),
            EditAction::Submit("hello\nworld".into())
        );
    }

    #[test]
    fn multiline_paste_above_the_line_limit_is_abbreviated() {
        let mut editor = Editor::default();
        let pasted = "a\n".repeat(LONG_PASTE_LINES - 1) + "a";
        editor.paste(&pasted);
        assert_eq!(
            editor.text(),
            format!("[Pasted #1 {} characters]", pasted.chars().count())
        );
        assert_eq!(editor.expand_pastes(&editor.text()), pasted);
    }

    #[test]
    fn take_text_expands_a_long_paste() {
        let mut editor = Editor::default();
        let pasted = "y".repeat(LONG_PASTE_CHARS + 1);
        editor.paste("before ");
        editor.paste(&pasted);
        assert_eq!(editor.take_text(), Some(format!("before {pasted}")));
    }
}
