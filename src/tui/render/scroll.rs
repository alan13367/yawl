//! The transcript scroll rail and the "Scroll to bottom" pill shown while the
//! view is scrolled up.

use crate::config::UiColor;

use super::super::state::{JumpButton, ScrollGeometry, scroll_bar_position, scroll_bar_span};
use super::super::{ViewState, markdown, theme};
use super::foreground_color;

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

/// Rail glyphs, both centered in the cell so the thumb rides on the track.
const SCROLL_TRACK: &str = "│";
const SCROLL_THUMB: &str = "┃";

/// How far the hovered thumb's halo fades the accent toward the background,
/// in percent.
const THUMB_HALO_FADE: u8 = 70;

/// Background behind a hovered or dragged thumb: a faint accent halo that
/// widens the line to the whole rail cell.
fn thumb_halo(accent: UiColor) -> String {
    let accent = theme::ink(accent);
    let target = if theme::is_light() { 255 } else { 0 };
    format!(
        "\x1b[48;2;{};{};{}m",
        mix_channel(accent.red, target, THUMB_HALO_FADE),
        mix_channel(accent.green, target, THUMB_HALO_FADE),
        mix_channel(accent.blue, target, THUMB_HALO_FADE)
    )
}

/// Appends the scroll rail to transcript rows drawn one cell narrower than
/// `columns`. While the transcript can scroll, the rail is a thin track with
/// the thumb over it, widened by a halo while the pointer rests on the rail
/// or drags it. Otherwise the cell stays blank, so the text keeps its width
/// when the bar auto-hides or the transcript starts to overflow.
pub(in crate::tui) fn append_scroll_rail(
    region: &mut [String],
    state: &mut ViewState,
    total_lines: usize,
    max_scroll: usize,
    columns: usize,
) {
    let height = region.len();
    if !state.show_scroll_bar || height == 0 || max_scroll == 0 || total_lines <= height {
        state.scroll_geometry = None;
        state.scroll_bar_hovered = false;
        for line in region {
            line.push_str("\x1b[0m ");
        }
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
    let track = format!(
        "\x1b[0m{}{SCROLL_TRACK}\x1b[0m",
        theme::palette().scroll_track
    );
    let halo = if state.scroll_bar_hovered || state.scroll_bar_drag.is_some() {
        thumb_halo(state.accent_color)
    } else {
        String::new()
    };
    let thumb = format!(
        "\x1b[0m{halo}{}{SCROLL_THUMB}\x1b[0m",
        foreground_color(state.accent_color)
    );
    for (row, line) in region.iter_mut().enumerate() {
        debug_assert_eq!(markdown::visible_width(line) + 1, columns);
        line.push_str(if (start..start + thumb_length).contains(&row) {
            &thumb
        } else {
            &track
        });
    }
}
