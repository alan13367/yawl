//! The transcript scroll bar and the "Scroll to bottom" pill shown while the
//! view is scrolled up.

use crate::config::UiColor;

use super::super::state::{JumpButton, ScrollGeometry, scroll_bar_position, scroll_bar_span};
use super::super::{ViewState, markdown, theme};
use super::picker_is_plan_handoff;

const JUMP_LABEL: &str = " ↓ Scroll to bottom ";

/// Rec. 601 luminance above which the pill uses dark text. Hover uses the
/// same cutoff to darken a light fill and lighten a dark one.
const JUMP_LIGHT_LUMA: u32 = 140_000;

/// How far a hovered pill moves toward black or white, in percent.
const JUMP_HOVER_SHIFT: u8 = 24;

fn luminance(color: UiColor) -> u32 {
    u32::from(color.red) * 299 + u32::from(color.green) * 587 + u32::from(color.blue) * 114
}

fn mix_channel(value: u8, target: u8, percent: u8) -> u8 {
    let value = u16::from(value);
    let target = u16::from(target);
    let percent = u16::from(percent);
    let mixed = value * (100 - percent) + target * percent;
    u8::try_from(mixed / 100).unwrap_or(u8::MAX)
}

fn jump_text_color(color: UiColor) -> &'static str {
    if luminance(color) > JUMP_LIGHT_LUMA {
        "\x1b[38;2;18;18;22m"
    } else {
        "\x1b[38;2;245;245;245m"
    }
}

fn jump_fill(accent: UiColor, hovered: bool) -> UiColor {
    if !hovered {
        return accent;
    }
    let target = if luminance(accent) > JUMP_LIGHT_LUMA {
        0
    } else {
        255
    };
    UiColor::new(
        mix_channel(accent.red, target, JUMP_HOVER_SHIFT),
        mix_channel(accent.green, target, JUMP_HOVER_SHIFT),
        mix_channel(accent.blue, target, JUMP_HOVER_SHIFT),
    )
}

/// Draws a "Scroll to bottom" pill centered on the last transcript row while
/// the view is scrolled up, and records its hit area. The pointer shifts the
/// fill so the control reads as hovered without moving or resizing.
pub(in crate::tui) fn render_jump_button(
    region: &mut [String],
    state: &mut ViewState,
    columns: usize,
) {
    let width = markdown::visible_width(JUMP_LABEL);
    let Some(row) = region.len().checked_sub(1) else {
        state.follow.pointer = None;
        return;
    };
    if columns < width + 2 {
        state.follow.pointer = None;
        return;
    }
    let left = (columns - width) / 2;
    let button = JumpButton {
        top: row,
        bottom: row + 1,
        left,
        right: left + width,
    };
    let hovered = state
        .follow
        .pointer
        .is_some_and(|(column, pointer_row)| button.contains(pointer_row, column));
    let accent = theme::ink(state.accent_color);
    let fill = jump_fill(accent, hovered);
    let pill = format!(
        "\x1b[48;2;{};{};{}m{}\x1b[1m{JUMP_LABEL}\x1b[0m",
        fill.red,
        fill.green,
        fill.blue,
        jump_text_color(accent)
    );
    region[row] = markdown::overlay_at(&region[row], left, &pill);
    state.follow.button = Some(button);
}

/// Thumb shading as a fraction of the accent color.
const SCROLL_THUMB_INTENSITY: f32 = 0.65;

fn shaded_background(color: UiColor, intensity: f32) -> String {
    let channel = |value: u8| (f32::from(value) * intensity).round() as u8;
    format!(
        "\x1b[48;2;{};{};{}m",
        channel(color.red),
        channel(color.green),
        channel(color.blue)
    )
}

pub(in crate::tui) fn apply_scroll_bar(
    region: &mut [String],
    state: &mut ViewState,
    total_lines: usize,
    max_scroll: usize,
    columns: usize,
) {
    let height = region.len();
    if !state.show_scroll_bar
        || state
            .picker
            .as_ref()
            .is_some_and(|picker| !picker_is_plan_handoff(picker))
        || max_scroll == 0
        || height == 0
        || total_lines <= height
        || columns == 0
    {
        state.scroll_geometry = None;
        return;
    }
    let (thumb_length, travel) = scroll_bar_span(height, total_lines);
    state.scroll_geometry = Some(ScrollGeometry {
        rows: height,
        columns,
        max_scroll,
        travel,
        thumb_length,
    });
    let start = scroll_bar_position(travel, max_scroll, state.scroll_offset);
    let thumb = shaded_background(state.accent_color, SCROLL_THUMB_INTENSITY);
    for (row, line) in region.iter_mut().enumerate() {
        debug_assert_eq!(markdown::visible_width(line), columns);
        if row >= start && row < start + thumb_length {
            *line = markdown::overlay_last_cell_background(line, &thumb);
        }
    }
}
