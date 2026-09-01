//! Multiline input editor with cursor movement, kill keys, paste, and
//! command history.

use std::cell::Cell;

use unicode_width::UnicodeWidthChar;

use super::clipboard::{ClipboardStore, StagedImage};
use super::events::Key;

const LONG_PASTE_CHARS: usize = 400;
const LONG_PASTE_LINES: usize = 8;

/// One editor submission with any staged clipboard images.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    pub(super) text: String,
    images: Vec<StagedImage>,
}

impl Submission {
    pub(super) fn new(text: String, images: Vec<StagedImage>) -> Self {
        Self { text, images }
    }

    pub(super) fn has_images(&self) -> bool {
        !self.referenced_images().is_empty()
    }

    pub(super) fn set_text(&mut self, text: String) {
        self.text = text;
    }

    pub(super) fn turn_input(&self, text: String) -> Result<crate::provider::TurnInput, String> {
        let mut images = Vec::new();
        for staged in self.referenced_images() {
            let bytes = std::fs::read(&staged.path)
                .map_err(|error| format!("cannot read {}: {error}", staged.path.display()))?;
            if bytes.len() > crate::image::MAX_IMAGE_BYTES {
                return Err(format!(
                    "{} exceeds the {}-byte image limit",
                    staged.path.display(),
                    crate::image::MAX_IMAGE_BYTES
                ));
            }
            let media_type = crate::image::media_type(&bytes).ok_or_else(|| {
                format!("{} is no longer a supported image", staged.path.display())
            })?;
            if media_type != staged.media_type {
                return Err(format!(
                    "{} changed after it was pasted",
                    staged.path.display()
                ));
            }
            images.push(crate::image::encode(media_type, &bytes));
        }
        Ok(crate::provider::TurnInput { text, images })
    }

    fn referenced_images(&self) -> Vec<&StagedImage> {
        let mut images = self
            .images
            .iter()
            .filter_map(|image| {
                self.text
                    .find(&image.marker())
                    .map(|offset| (offset, image))
            })
            .collect::<Vec<_>>();
        images.sort_by_key(|(offset, _)| *offset);
        images.into_iter().map(|(_, image)| image).collect()
    }
}

impl From<String> for Submission {
    fn from(text: String) -> Self {
        Self::new(text, Vec::new())
    }
}

impl From<&str> for Submission {
    fn from(text: &str) -> Self {
        text.to_string().into()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum EditAction {
    None,
    Submit(Submission),
    Steer(Submission),
}

#[derive(Debug)]
pub struct InputLayout {
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

#[derive(Debug, Clone, Copy)]
struct CursorPosition {
    row: usize,
    column: usize,
}

struct BufferLayout {
    lines: Vec<String>,
    positions: Vec<CursorPosition>,
}

#[derive(Default)]
pub struct Editor {
    buffer: Vec<char>,
    cursor: usize,
    layout_width: Cell<usize>,
    preferred_column: Option<usize>,
    history: Vec<Submission>,
    history_index: Option<usize>,
    history_draft: Option<Submission>,
    pastes: Vec<String>,
    /// `@` mention tags inserted by completion: `(display, relative path)`.
    /// The short display stays visible; the path is substituted on submit.
    mentions: Vec<(String, String)>,
    images: Vec<StagedImage>,
    next_image_id: usize,
    clipboard: ClipboardStore,
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

    pub(super) fn next_image_id(&self) -> usize {
        self.next_image_id.saturating_add(1)
    }

    pub(super) fn can_add_image(&self) -> bool {
        Submission::new(self.text(), self.images.clone())
            .referenced_images()
            .len()
            < 5
    }

    pub(super) fn insert_image(&mut self, image: StagedImage) {
        self.leave_history();
        self.next_image_id = self.next_image_id.max(image.id);
        let marker = image.marker();
        self.images.push(image);
        self.insert_chars(marker.chars());
    }

    pub(super) fn paste_clipboard_image(&mut self, supports_images: bool) -> Result<(), String> {
        if !supports_images {
            return Err("the selected model does not accept image input".into());
        }
        if !self.can_add_image() {
            return Err("a prompt can contain at most 5 images".into());
        }
        let image = self.clipboard.paste_image(self.next_image_id())?;
        self.insert_image(image);
        Ok(())
    }

    pub(super) fn restore_submission(&mut self, submission: Submission) {
        self.buffer = submission.text.chars().collect();
        self.cursor = self.buffer.len();
        self.preferred_column = None;
        self.images = submission.images;
        self.history_index = None;
        self.history_draft = None;
    }

    /// Replaces long-paste placeholders with the original pasted text.
    pub fn expand_pastes(&self, text: &str) -> String {
        expand_paste_placeholders(text, &self.pastes)
    }

    /// Prepares submitted text for the agent: `@` mention tags become
    /// relative paths, then long-paste placeholders are expanded. Mentions
    /// run first so pasted content is never rewritten.
    pub fn expand_submission(&self, text: &str) -> String {
        self.expand_pastes(&self.expand_mentions(text))
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

    /// Query of the `@` mention token the cursor is editing, without the
    /// leading `@`.
    pub fn mention_prefix(&self) -> Option<String> {
        self.mention_token_range()
            .map(|(start, end)| self.buffer[start + 1..end].iter().collect())
    }

    /// Bounds of the whitespace-delimited `@` token containing the cursor.
    fn mention_token_range(&self) -> Option<(usize, usize)> {
        let start = self.buffer[..self.cursor]
            .iter()
            .rposition(|character| character.is_whitespace())
            .map_or(0, |index| index + 1);
        if self.buffer.get(start) != Some(&'@') {
            return None;
        }
        let end = self.buffer[start..]
            .iter()
            .position(|character| character.is_whitespace())
            .map_or(self.buffer.len(), |index| start + index);
        (self.cursor <= end).then_some((start, end))
    }

    /// Replaces the mention token under the cursor with a short display tag
    /// for `path` and records the tag for expansion on submit.
    pub fn complete_mention(&mut self, path: &str) {
        let Some((start, end)) = self.mention_token_range() else {
            return;
        };
        let display = self.unique_mention_display(path);
        let replacement: Vec<char> = format!("@{display}").chars().collect();
        let length = replacement.len();
        self.buffer.splice(start..end, replacement);
        self.cursor = start + length;
        if !self
            .buffer
            .get(self.cursor)
            .is_some_and(|character| character.is_whitespace())
        {
            self.buffer.insert(self.cursor, ' ');
        }
        self.cursor += 1;
        if !self
            .mentions
            .iter()
            .any(|(existing, existing_path)| existing == &display && existing_path == path)
        {
            self.mentions.push((display, path.to_string()));
        }
        self.leave_history();
    }

    /// Shortest path suffix that does not collide with a tag already mapped
    /// to a different file, so `@render.rs` and `@other/render.rs` coexist.
    fn unique_mention_display(&self, path: &str) -> String {
        let components: Vec<&str> = path.split('/').collect();
        for take in 1..=components.len() {
            let display = components[components.len() - take..].join("/");
            let conflict = self
                .mentions
                .iter()
                .any(|(existing, existing_path)| existing == &display && existing_path != path);
            if !conflict {
                return display;
            }
        }
        path.to_string()
    }

    /// Replaces recorded `@` mention tags with `@` plus the relative path.
    /// Unrecognized `@` tokens pass through untouched.
    pub fn expand_mentions(&self, text: &str) -> String {
        if self.mentions.is_empty() || !text.contains('@') {
            return text.to_string();
        }
        let mut mentions: Vec<&(String, String)> = self.mentions.iter().collect();
        mentions.sort_by_key(|(display, _)| std::cmp::Reverse(display.chars().count()));
        let mut result = String::with_capacity(text.len());
        let mut rest = text;
        let mut at_token_start = true;
        while let Some(offset) = rest.find('@') {
            let (before, from_marker) = rest.split_at(offset);
            result.push_str(before);
            let token_start =
                (at_token_start && before.is_empty()) || before.ends_with(char::is_whitespace);
            let matched = token_start
                .then(|| {
                    mentions.iter().find(|(display, _)| {
                        from_marker[1..].starts_with(display.as_str())
                            && from_marker[1 + display.len()..]
                                .chars()
                                .next()
                                .is_none_or(char::is_whitespace)
                    })
                })
                .flatten();
            if let Some((display, path)) = matched {
                result.push('@');
                result.push_str(path);
                rest = &from_marker[1 + display.len()..];
            } else {
                result.push('@');
                rest = &from_marker[1..];
            }
            at_token_start = false;
        }
        result.push_str(rest);
        result
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
        self.preferred_column = None;
        self.history_index = None;
        self.history_draft = None;
        self.images.clear();
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
        if !matches!(key, Key::Up | Key::Down) {
            self.preferred_column = None;
        }
        match key {
            Key::Char(character) => self.insert(character),
            Key::Newline => self.insert('\n'),
            Key::Enter => return self.submit(),
            Key::Steer | Key::Ctrl('g') => return self.steer(),
            Key::Backspace => self.backspace(),
            Key::Delete => self.delete(),
            Key::Left => self.cursor = self.cursor.saturating_sub(1),
            Key::Right => self.cursor = (self.cursor + 1).min(self.buffer.len()),
            Key::Home | Key::Ctrl('a') => self.cursor = self.line_start(),
            Key::End | Key::Ctrl('e') => self.cursor = self.line_end(),
            Key::Up => {
                if !self.move_vertical(true) {
                    self.preferred_column = None;
                    self.history_previous();
                }
            }
            Key::Down => {
                if !self.move_vertical(false) {
                    self.preferred_column = None;
                    self.history_next();
                }
            }
            Key::Ctrl('u') => self.kill_to_line_start(),
            Key::Ctrl('k') => self.kill_to_line_end(),
            Key::Ctrl('w') => self.kill_previous_word(),
            Key::Tab => self.paste("    "),
            Key::PageUp | Key::PageDown | Key::Escape | Key::Ctrl(_) | Key::Super(_) => {}
        }
        EditAction::None
    }

    pub fn layout(&self, width: usize) -> InputLayout {
        self.layout_with_buffer(width, &self.buffer)
    }

    pub fn masked_layout(&self, width: usize) -> InputLayout {
        let masked = self
            .buffer
            .iter()
            .map(|character| if *character == '\n' { '\n' } else { '•' })
            .collect::<Vec<_>>();
        self.layout_with_buffer(width, &masked)
    }

    fn layout_with_buffer(&self, width: usize, buffer: &[char]) -> InputLayout {
        let width = width.max(3);
        self.layout_width.set(width);
        let layout = buffer_layout(buffer, width);
        let cursor = layout.positions[self.cursor];
        InputLayout {
            lines: layout.lines,
            cursor_row: cursor.row,
            cursor_col: cursor.column,
        }
    }

    fn move_vertical(&mut self, upward: bool) -> bool {
        let width = self.layout_width.get();
        if width < 3 {
            return false;
        }
        let positions = buffer_layout(&self.buffer, width).positions;
        let current = positions[self.cursor];
        let target_row = if upward {
            current.row.checked_sub(1)
        } else {
            current.row.checked_add(1).filter(|row| {
                positions
                    .last()
                    .is_some_and(|position| *row <= position.row)
            })
        };
        let Some(target_row) = target_row else {
            return false;
        };
        let preferred_column = *self.preferred_column.get_or_insert(current.column);
        let Some((target, _)) = positions
            .iter()
            .enumerate()
            .filter(|(_, position)| position.row == target_row)
            .min_by_key(|(_, position)| position.column.abs_diff(preferred_column))
        else {
            return false;
        };
        self.cursor = target;
        true
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
        self.take_submission()
            .map_or(EditAction::None, EditAction::Submit)
    }

    fn steer(&mut self) -> EditAction {
        self.take_submission()
            .map_or(EditAction::None, EditAction::Steer)
    }

    fn take_submission(&mut self) -> Option<Submission> {
        let text: String = self.buffer.iter().collect();
        if text.trim().is_empty() {
            return None;
        }
        let submission = Submission::new(text, self.images.clone());
        if self.history.last() != Some(&submission) {
            self.history.push(submission.clone());
        }
        self.clear();
        Some(submission)
    }

    fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_index {
            None => {
                self.history_draft = Some(Submission::new(self.text(), self.images.clone()));
                self.history.len() - 1
            }
            Some(index) => index.saturating_sub(1),
        };
        self.history_index = Some(next);
        self.buffer = self.history[next].text.chars().collect();
        self.images.clone_from(&self.history[next].images);
        self.cursor = self.buffer.len();
    }

    fn history_next(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.history.len() {
            let next = index + 1;
            self.history_index = Some(next);
            self.buffer = self.history[next].text.chars().collect();
            self.images.clone_from(&self.history[next].images);
        } else {
            self.history_index = None;
            if let Some(draft) = self.history_draft.take() {
                self.buffer = draft.text.chars().collect();
                self.images = draft.images;
            } else {
                self.buffer.clear();
                self.images.clear();
            }
        }
        self.cursor = self.buffer.len();
    }

    fn leave_history(&mut self) {
        self.preferred_column = None;
        self.history_index = None;
        self.history_draft = None;
    }
}

fn displayed_character(character: char) -> char {
    if character.is_control() {
        '�'
    } else {
        character
    }
}

fn wraps_before(column: usize, character_width: usize, width: usize) -> bool {
    column > 2 && column.saturating_add(character_width) > width
}

fn word_width(buffer: &[char], start: usize) -> usize {
    buffer[start..]
        .iter()
        .copied()
        .take_while(|character| !character.is_whitespace())
        .map(displayed_character)
        .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
        .sum()
}

fn should_wrap_word(buffer: &[char], index: usize, column: usize, width: usize) -> bool {
    let character = buffer[index];
    if character.is_whitespace()
        || index
            .checked_sub(1)
            .is_some_and(|previous| !buffer[previous].is_whitespace())
    {
        return false;
    }
    let word_width = word_width(buffer, index);
    column > 2 && word_width <= width.saturating_sub(2) && column.saturating_add(word_width) > width
}

fn buffer_layout(buffer: &[char], width: usize) -> BufferLayout {
    let mut lines = Vec::new();
    let mut positions = Vec::with_capacity(buffer.len().saturating_add(1));
    let mut line = String::from("> ");
    let mut column = 2usize;

    for (index, character) in buffer.iter().copied().enumerate() {
        if character == '\n' {
            positions.push(CursorPosition {
                row: lines.len(),
                column,
            });
            lines.push(line);
            line = String::from("  ");
            column = 2;
            continue;
        }
        let displayed = displayed_character(character);
        let character_width = UnicodeWidthChar::width(displayed).unwrap_or(0);
        if character.is_whitespace() && wraps_before(column, character_width, width) {
            lines.push(line);
            line = String::from("  ");
            column = 2;
            positions.push(CursorPosition {
                row: lines.len(),
                column,
            });
            continue;
        }
        if should_wrap_word(buffer, index, column, width)
            || wraps_before(column, character_width, width)
        {
            lines.push(line);
            line = String::from("  ");
            column = 2;
        }
        positions.push(CursorPosition {
            row: lines.len(),
            column,
        });
        line.push(displayed);
        column = column.saturating_add(character_width);
    }
    positions.push(CursorPosition {
        row: lines.len(),
        column,
    });
    lines.push(line);
    BufferLayout { lines, positions }
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

    fn staged_image(id: usize, suffix: &str) -> (StagedImage, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "yawl-input-image-{}-{id}-{suffix}.png",
            std::process::id()
        ));
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(suffix.as_bytes());
        std::fs::write(&path, bytes).expect("write image");
        (
            StagedImage {
                id,
                path: path.clone(),
                media_type: "image/png".into(),
            },
            path,
        )
    }

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
    fn ctrl_g_is_the_legacy_terminal_steering_shortcut() {
        let mut editor = Editor::default();
        editor.paste("change direction");
        assert_eq!(
            editor.handle_key(Key::Ctrl('g')),
            EditAction::Steer("change direction".into())
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
    fn image_markers_control_order_and_deduplicate_payloads() {
        let (first, first_path) = staged_image(1, "first");
        let (second, second_path) = staged_image(2, "second");
        let submission = Submission::new(
            "[Image #2] compare [Image #1] with [Image #2]".into(),
            vec![first, second],
        );

        let input = submission
            .turn_input(submission.text.clone())
            .expect("images");
        assert_eq!(input.images.len(), 2);
        assert_ne!(input.images[0].data, input.images[1].data);
        let _ = std::fs::remove_file(first_path);
        let _ = std::fs::remove_file(second_path);
    }

    #[test]
    fn deleting_marker_detaches_image_and_history_restores_it() {
        let (image, path) = staged_image(1, "history");
        let detached = Submission::new("marker removed".into(), vec![image.clone()]);
        assert!(!detached.has_images());

        let mut editor = Editor::default();
        editor.insert_image(image);
        assert!(matches!(
            editor.handle_key(Key::Enter),
            EditAction::Submit(_)
        ));
        editor.handle_key(Key::Up);
        let EditAction::Submit(recalled) = editor.handle_key(Key::Enter) else {
            panic!("expected recalled submission");
        };
        assert!(recalled.has_images());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn editor_enforces_five_referenced_images() {
        let mut editor = Editor::default();
        let mut paths = Vec::new();
        for id in 1..=5 {
            let (image, path) = staged_image(id, &id.to_string());
            paths.push(path);
            editor.insert_image(image);
        }
        assert!(!editor.can_add_image());
        for path in paths {
            let _ = std::fs::remove_file(path);
        }
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
    fn layout_moves_a_complete_word_to_the_next_row() {
        let mut editor = Editor::default();
        editor.paste("hi palabra");

        let layout = editor.layout(10);

        assert_eq!(layout.lines, ["> hi ", "  palabra"]);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 9));
    }

    #[test]
    fn layout_absorbs_whitespace_at_a_wrap_boundary() {
        let mut editor = Editor::default();
        editor.paste("abcdefgh ijklmnop");

        let layout = editor.layout(10);

        assert_eq!(layout.lines, ["> abcdefgh", "  ijklmnop"]);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 10));
    }

    #[test]
    fn layout_wraps_wide_characters_before_the_right_edge() {
        let mut editor = Editor::default();
        editor.paste("abc界def");

        let layout = editor.layout(6);

        assert_eq!(layout.lines, ["> abc", "  界de", "  f"]);
        assert_eq!((layout.cursor_row, layout.cursor_col), (2, 3));
    }

    #[test]
    fn up_and_down_move_between_wrapped_input_rows() {
        let mut editor = Editor::default();
        editor.paste("abcdefghijkl");
        assert_eq!(
            (editor.layout(7).cursor_row, editor.layout(7).cursor_col),
            (2, 4)
        );

        editor.handle_key(Key::Up);
        assert_eq!(
            (editor.layout(7).cursor_row, editor.layout(7).cursor_col),
            (1, 4)
        );

        editor.handle_key(Key::Up);
        assert_eq!(
            (editor.layout(7).cursor_row, editor.layout(7).cursor_col),
            (0, 4)
        );

        editor.handle_key(Key::Down);
        assert_eq!(
            (editor.layout(7).cursor_row, editor.layout(7).cursor_col),
            (1, 4)
        );
    }

    #[test]
    fn vertical_movement_crosses_newlines_and_preserves_the_target_column() {
        let mut editor = Editor::default();
        editor.paste("abcdef\nx\nuvwxyz");
        editor.layout(20);

        editor.handle_key(Key::Up);
        assert_eq!(
            (editor.layout(20).cursor_row, editor.layout(20).cursor_col),
            (1, 3)
        );
        editor.handle_key(Key::Up);
        assert_eq!(
            (editor.layout(20).cursor_row, editor.layout(20).cursor_col),
            (0, 8)
        );

        editor.handle_key(Key::Char('!'));
        assert_eq!(editor.text(), "abcdef!\nx\nuvwxyz");
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
    fn mention_prefix_tracks_the_token_under_the_cursor() {
        let mut editor = Editor::default();
        editor.paste("fix @ren please");
        assert_eq!(editor.mention_prefix(), None);
        editor.handle_key(Key::Home);
        for _ in 0..8 {
            editor.handle_key(Key::Right);
        }
        assert_eq!(editor.mention_prefix().as_deref(), Some("ren"));
    }

    #[test]
    fn mention_prefix_ignores_at_signs_inside_words() {
        let mut editor = Editor::default();
        editor.paste("mail me@example.com");
        assert_eq!(editor.mention_prefix(), None);
    }

    #[test]
    fn completed_mentions_expand_to_relative_paths_on_submit() {
        let mut editor = Editor::default();
        editor.paste("compare @ren");
        editor.complete_mention("src/tui/render.rs");
        assert_eq!(editor.text(), "compare @render.rs ");

        editor.paste("with @ren");
        editor.complete_mention("src/other/render.rs");
        assert_eq!(editor.text(), "compare @render.rs with @other/render.rs ");
        assert_eq!(
            editor.expand_submission(&editor.text()),
            "compare @src/tui/render.rs with @src/other/render.rs "
        );
    }

    #[test]
    fn unrecorded_at_tokens_pass_through_unchanged() {
        let mut editor = Editor::default();
        editor.paste("ping @ren");
        editor.complete_mention("src/tui/render.rs");
        assert_eq!(
            editor.expand_submission("see @render.rs and @unknown and me@example.com"),
            "see @src/tui/render.rs and @unknown and me@example.com"
        );
    }

    #[test]
    fn mention_expansion_requires_a_full_token_match() {
        let mut editor = Editor::default();
        editor.paste("@ren");
        editor.complete_mention("src/tui/render.rs");
        assert_eq!(
            editor.expand_submission("@render.rs.bak stays"),
            "@render.rs.bak stays"
        );
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
