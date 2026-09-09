//! Welcome animation rendering.
//!
//! The frame builder coordinates each frame; this child renders the
//! centered startup wordmark and its typing animation.

use crate::config::UiColor;

use super::super::markdown;
use super::{foreground_color, status_style};

/// Block letters for typical terminal widths. Falls back to the smaller
/// figlet wordmark, then to the word itself, when the region is tight.
const WELCOME_LOGO_LARGE: &[&str] = &[
    "██    ██   █████   ██     ██  ██",
    " ██  ██   ██   ██  ██     ██  ██",
    "  ████    ███████  ██  █  ██  ██",
    "   ██     ██   ██  ██ ███ ██  ██",
    "   ██     ██   ██   ███ ███   ███████",
];

const WELCOME_LOGO_SMALL: &[&str] = &[
    r"__   __            _",
    r"\ \ / /_ ___      _| |",
    r" \ V / _` \ \ /\ / / |",
    r"  | | (_| |\ V  V /| |",
    r"  |_|\__,_| \_/\_/ |_|",
];

fn logo_fits(lines: &[&str], width: usize) -> bool {
    lines
        .iter()
        .map(|line| markdown::visible_width(line))
        .max()
        .unwrap_or(0)
        <= width
}

fn welcome_logo(width: usize, height: usize) -> &'static [&'static str] {
    const COMPACT: &[&str] = &["Yawl"];
    if height >= WELCOME_LOGO_LARGE.len() && logo_fits(WELCOME_LOGO_LARGE, width) {
        WELCOME_LOGO_LARGE
    } else if height >= WELCOME_LOGO_SMALL.len() && logo_fits(WELCOME_LOGO_SMALL, width) {
        WELCOME_LOGO_SMALL
    } else {
        COMPACT
    }
}

/// Columns of the wordmark (and hint characters) revealed per 100ms tick.
const WELCOME_COLUMNS_PER_TICK: usize = 3;

/// Ticks after which the large wordmark and hint have finished typing.
pub(in crate::tui) const WELCOME_ANIMATION_TICKS: usize = 40;

fn pad_to_width(text: &str, width: usize) -> String {
    let visible = markdown::visible_width(text);
    if visible >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - visible))
    }
}

fn center_styled(text: &str, style: &str, width: usize) -> String {
    let visible = markdown::visible_width(text);
    let left = width.saturating_sub(visible) / 2;
    markdown::fit_width(&format!("{}{style}{text}\x1b[0m", " ".repeat(left)), width)
}

fn prefix_visible(text: &str, columns: usize) -> &str {
    if columns == 0 {
        return "";
    }
    let mut end = 0;
    for (index, character) in text.char_indices() {
        let next = index + character.len_utf8();
        if markdown::visible_width(&text[..next]) > columns {
            break;
        }
        end = next;
    }
    &text[..end]
}

fn typed_amount(tick: usize, total: usize) -> usize {
    tick.saturating_add(1)
        .saturating_mul(WELCOME_COLUMNS_PER_TICK)
        .min(total)
}

fn typed_hint(tick: usize, logo_ticks: usize, hint: &str) -> &str {
    let Some(elapsed) = tick.saturating_add(1).checked_sub(logo_ticks) else {
        return "";
    };
    if elapsed == 0 {
        return "";
    }
    let keep = typed_amount(elapsed - 1, hint.chars().count());
    hint.char_indices()
        .nth(keep)
        .map_or(hint, |(index, _)| &hint[..index])
}

fn mask_logo_line(padded: &str, revealed: usize, show_cursor: bool) -> String {
    let total = markdown::visible_width(padded);
    if revealed >= total {
        return padded.to_string();
    }
    let mut line = String::with_capacity(padded.len() + 1);
    line.push_str(prefix_visible(padded, revealed));
    let width = markdown::visible_width(&line);
    if width < revealed {
        line.extend(std::iter::repeat_n(' ', revealed - width));
    }
    if show_cursor {
        line.push('|');
    }
    pad_to_width(&line, total)
}

pub(super) fn render_welcome(
    accent: UiColor,
    width: usize,
    height: usize,
    tick: usize,
) -> Vec<String> {
    let color = format!("{}\x1b[1m", foreground_color(accent));
    let logo = welcome_logo(width, height);
    let block_width = logo
        .iter()
        .map(|line| markdown::visible_width(line))
        .max()
        .unwrap_or(0);
    let revealed = typed_amount(tick, block_width);
    let show_cursor = revealed < block_width && tick.is_multiple_of(2);
    let mut content: Vec<String> = logo
        .iter()
        .map(|line| {
            let padded = pad_to_width(line, block_width);
            let typed = mask_logo_line(&padded, revealed, show_cursor);
            center_styled(&typed, &color, width)
        })
        .collect();
    const HINT: &str = "Type /help for commands.";
    if height >= content.len() + 2 && markdown::visible_width(HINT) <= width {
        let blank = markdown::fit_width("", width);
        content.push(blank.clone());
        let logo_ticks = block_width.div_ceil(WELCOME_COLUMNS_PER_TICK);
        let hint = typed_hint(tick, logo_ticks, HINT);
        if hint.is_empty() {
            content.push(blank);
        } else {
            content.push(center_styled(hint, &status_style(accent), width));
        }
    }
    let blank = markdown::fit_width("", width);
    let top = height.saturating_sub(content.len()) / 2;
    let mut lines = Vec::with_capacity(height);
    lines.resize(top, blank.clone());
    lines.extend(content);
    lines.resize(height, blank);
    lines
}
