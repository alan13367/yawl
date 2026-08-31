//! Arrow-key list selection for the setup wizard. Draws in place with ANSI
//! cursor movement; no alternate screen, so the transcript stays linear.

use std::io::{self, IsTerminal, Read, Write};

use crate::error::Error;

use super::terminal;

/// Options drawn at once before the list scrolls.
const MAX_VISIBLE: usize = 10;
/// Longest combined row, kept under common 80-column terminals.
const MAX_LABEL_CHARS: usize = 42;
const MAX_HINT_CHARS: usize = 32;

/// One row of a selector list: a label plus a dim hint shown after it.
#[derive(Clone)]
pub(super) struct Choice {
    pub(super) label: String,
    pub(super) hint: String,
}

impl Choice {
    pub(super) fn new(label: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            hint: hint.into(),
        }
    }
}

/// Shows a single-choice list. Arrow keys or j/k move, Enter picks, Esc or q
/// cancels. Returns the chosen index, or `None` when canceled. Falls back to
/// a numbered prompt that re-asks on bad input when stdin is not a terminal.
///
/// # Errors
///
/// Returns an error when terminal setup or input fails, and
/// [`Error::Interrupted`] when the user presses Ctrl+C.
pub(super) fn select(title: &str, choices: &[Choice]) -> Result<Option<usize>, Error> {
    if choices.is_empty() {
        return Ok(None);
    }
    if io::stdin().is_terminal() {
        select_interactive(title, choices)
    } else {
        select_numbered(title, choices)
    }
}

fn select_interactive(title: &str, choices: &[Choice]) -> Result<Option<usize>, Error> {
    let mut out = io::stdout();
    let _raw = crate::terminal_mode::RawMode::enter()?;
    let visible = choices.len().min(MAX_VISIBLE);
    let mut cursor = 0usize;
    draw_initial(&mut out, title, choices, cursor, visible)?;
    // Lines from the title through the last option; the cursor rests one
    // line below the block after every draw.
    let block_height = visible + 1;
    loop {
        match read_key()? {
            Key::Enter => {
                collapse(&mut out, block_height, visible, title, &choices[cursor])?;
                return Ok(Some(cursor));
            }
            Key::Cancel => {
                clear_block(&mut out, block_height, visible)?;
                return Ok(None);
            }
            Key::Up => {
                cursor = (cursor + choices.len() - 1) % choices.len();
                redraw(&mut out, choices, cursor, visible)?;
            }
            Key::Down => {
                cursor = (cursor + 1) % choices.len();
                redraw(&mut out, choices, cursor, visible)?;
            }
            Key::Other => {}
        }
    }
}

fn draw_initial(
    out: &mut impl Write,
    title: &str,
    choices: &[Choice],
    cursor: usize,
    visible: usize,
) -> Result<(), Error> {
    // Raw mode disables output post-processing, so every newline must be
    // an explicit \r\n or rows inherit the previous row's column.
    write!(out, "\x1b[?25l{title}")?;
    for index in visible_window(cursor, choices.len(), visible) {
        write!(out, "\r\n{}", render_row(&choices[index], index == cursor))?;
    }
    write!(out, "\r\n")?;
    out.flush()?;
    Ok(())
}

fn redraw(
    out: &mut impl Write,
    choices: &[Choice],
    cursor: usize,
    visible: usize,
) -> Result<(), Error> {
    // The title stays put; only the option rows are rewritten.
    write!(out, "\x1b[{visible}A")?;
    for index in visible_window(cursor, choices.len(), visible) {
        write!(
            out,
            "\r\x1b[2K{}\r\n",
            render_row(&choices[index], index == cursor)
        )?;
    }
    out.flush()?;
    Ok(())
}

/// Replaces the whole block with one line: `title: label`.
fn collapse(
    out: &mut impl Write,
    block_height: usize,
    visible: usize,
    title: &str,
    choice: &Choice,
) -> Result<(), Error> {
    write!(
        out,
        "\x1b[{block_height}A\r\x1b[2K{title}: {}\x1b[{visible}M\x1b[?25h\r\n",
        choice.label
    )?;
    out.flush()?;
    Ok(())
}

/// Removes the whole block without leaving output behind. The cursor ends
/// at column 0 of the line where the title was.
fn clear_block(out: &mut impl Write, block_height: usize, visible: usize) -> Result<(), Error> {
    write!(out, "\x1b[{block_height}A\r\x1b[2K\x1b[{visible}M\x1b[?25h")?;
    out.flush()?;
    Ok(())
}

/// The slice of choice indexes currently on screen. The window follows the
/// cursor and never scrolls past the ends of the list.
fn visible_window(cursor: usize, length: usize, visible: usize) -> std::ops::Range<usize> {
    if length <= visible {
        return 0..length;
    }
    let start = cursor
        .saturating_sub(visible.saturating_sub(1))
        .min(length - visible);
    start..start + visible
}

fn render_row(choice: &Choice, selected: bool) -> String {
    let label = truncate_chars(&choice.label, MAX_LABEL_CHARS);
    let hint = truncate_chars(&choice.hint, MAX_HINT_CHARS);
    if selected {
        format!("\x1b[1m ❯ {label} \x1b[0m \x1b[2m{hint}\x1b[0m")
    } else {
        format!("   {label}  \x1b[2m{hint}\x1b[0m")
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

enum Key {
    Up,
    Down,
    Enter,
    Cancel,
    Other,
}

/// Reads one logical key. Arrow keys arrive as escape sequences; a lone Esc
/// within the read timeout cancels.
fn read_key() -> Result<Key, Error> {
    loop {
        match read_byte() {
            Ok(Some(byte)) => return decode(byte),
            // VTIME timeout with no byte: keep waiting.
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

fn decode(byte: u8) -> Result<Key, Error> {
    match byte {
        b'\r' | b'\n' => Ok(Key::Enter),
        b'j' => Ok(Key::Down),
        b'k' => Ok(Key::Up),
        b'q' => Ok(Key::Cancel),
        0x1b => match read_byte() {
            // Lone Esc (nothing followed within the timeout) cancels.
            Ok(None) | Ok(Some(0x1b)) => Ok(Key::Cancel),
            Ok(Some(b'[')) | Ok(Some(b'O')) => match read_byte() {
                Ok(Some(b'A')) => Ok(Key::Up),
                Ok(Some(b'B')) => Ok(Key::Down),
                _ => Ok(Key::Other),
            },
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                if crate::interrupted() {
                    Err(Error::Interrupted)
                } else {
                    Ok(Key::Cancel)
                }
            }
            Err(error) => Err(Error::Io(error)),
            _ => Ok(Key::Other),
        },
        _ => Ok(Key::Other),
    }
}

fn read_byte() -> io::Result<Option<u8>> {
    let mut buffer = [0u8; 1];
    Ok((io::stdin().read(&mut buffer)? == 1).then_some(buffer[0]))
}

fn select_numbered(title: &str, choices: &[Choice]) -> Result<Option<usize>, Error> {
    loop {
        println!("{title}");
        for (index, choice) in choices.iter().enumerate() {
            println!("  {}. {}", index + 1, choice.label);
        }
        let answer = terminal::prompt("Choice number, or press Enter to cancel")?;
        if answer.is_empty() {
            return Ok(None);
        }
        if let Ok(number) = answer.parse::<usize>()
            && let Some(index) = number.checked_sub(1)
            && index < choices.len()
        {
            return Ok(Some(index));
        }
        println!("Enter a number from 1 through {}.", choices.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_covers_short_lists_and_follows_the_cursor() {
        assert_eq!(visible_window(0, 3, 10), 0..3);
        assert_eq!(visible_window(9, 30, 10), 0..10);
        assert_eq!(visible_window(10, 30, 10), 1..11);
        assert_eq!(visible_window(29, 30, 10), 20..30);
    }

    #[test]
    fn rows_mark_the_cursor_and_dim_the_hint() {
        let selected = render_row(&Choice::new("Ollama", "local models"), true);
        assert!(selected.contains("❯ Ollama"));
        assert!(selected.contains("\x1b[2mlocal models"));

        let plain = render_row(&Choice::new("Ollama", "local models"), false);
        assert!(!plain.contains('❯'));
        assert!(plain.contains("Ollama"));
    }

    #[test]
    fn truncation_keeps_rows_short_and_appends_an_ellipsis() {
        assert_eq!(truncate_chars("short", 8), "short");
        let long = truncate_chars(&"x".repeat(50), MAX_LABEL_CHARS);
        assert_eq!(long.chars().count(), MAX_LABEL_CHARS);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn draw_output_never_emits_a_bare_line_feed() {
        // Raw mode disables output post-processing, so a bare \n moves down
        // without returning to column 0 and rows stagger across the screen.
        let choices: Vec<Choice> = (0..3)
            .map(|index| Choice::new(format!("option {index}"), "hint"))
            .collect();

        let mut initial = Vec::new();
        draw_initial(&mut initial, "Provider", &choices, 1, 3).expect("draw should write");
        let initial = String::from_utf8(initial).expect("draw output is ANSI text");
        assert_eq!(
            initial.matches('\n').count(),
            initial.matches("\r\n").count(),
            "every newline needs a carriage return"
        );
        assert!(initial.starts_with("\x1b[?25lProvider\r\n"));

        let mut redrawn = Vec::new();
        redraw(&mut redrawn, &choices, 2, 3).expect("redraw should write");
        let redrawn = String::from_utf8(redrawn).expect("redraw output is ANSI text");
        assert_eq!(
            redrawn.matches('\n').count(),
            redrawn.matches("\r\n").count()
        );

        let mut collapsed = Vec::new();
        collapse(&mut collapsed, 4, 3, "Provider", &choices[2]).expect("collapse should write");
        let collapsed = String::from_utf8(collapsed).expect("collapse output is ANSI text");
        assert_eq!(
            collapsed.matches('\n').count(),
            collapsed.matches("\r\n").count()
        );
    }
}
