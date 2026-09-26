//! Thinking tags and reasoning traces, including their color and duration format.

use crate::config::UiColor;
use crate::provider::ReasoningKind;

use super::super::markdown;
use super::{SPINNER_FRAMES, foreground_color};

/// Renders the thinking tag: a spinner frame and `Thinking` while the
/// reasoning streams (the timer is not shown until it settles), then
/// `+ Thought: 4.2s` in an accent-derived colour, `- Thought: 4.2s` dimmed when
/// expanded.
pub(super) fn render_thinking_tag(
    elapsed: Option<std::time::Duration>,
    streaming: bool,
    expanded: bool,
    spinner_tick: usize,
    accent_color: UiColor,
    width: usize,
) -> Vec<String> {
    let accent = foreground_color(thinking_color(accent_color));
    let line = if streaming {
        let frame = SPINNER_FRAMES[spinner_tick % SPINNER_FRAMES.len()];
        format!("{accent}{frame} Thinking\x1b[0m")
    } else {
        let marker = if expanded { "-" } else { "+" };
        let time = elapsed
            .map(|elapsed| format!(": {}", format_thinking_elapsed(elapsed)))
            .unwrap_or_default();
        let dim = if expanded { "\x1b[2m" } else { "" };
        format!("{accent}{dim}{marker} Thought{time}\x1b[0m")
    };
    vec![markdown::fit_width(&line, width)]
}

/// Shift the accent hue by 45 degrees and bound saturation and brightness.
/// Neutral accents get a blue-gray tint rather than resembling reply text.
pub(super) fn thinking_color(accent: UiColor) -> UiColor {
    let [red, green, blue] = [accent.red, accent.green, accent.blue].map(f64::from);
    let max = red.max(green).max(blue);
    let min = red.min(green).min(blue);
    let chroma = max - min;
    let (hue, saturation) = if chroma < 16.0 {
        (210.0, 0.35)
    } else {
        let sector = if max == red {
            (green - blue) / chroma
        } else if max == green {
            (blue - red) / chroma + 2.0
        } else {
            (red - green) / chroma + 4.0
        };
        (
            (sector * 60.0 + 45.0).rem_euclid(360.0),
            (chroma / max).clamp(0.4, 0.65),
        )
    };
    let value = max.clamp(180.0, 205.0);
    let chroma = value * saturation;
    let secondary = chroma * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let (red, green, blue) = match hue as u16 {
        0..60 => (chroma, secondary, 0.0),
        60..120 => (secondary, chroma, 0.0),
        120..180 => (0.0, chroma, secondary),
        180..240 => (0.0, secondary, chroma),
        240..300 => (secondary, 0.0, chroma),
        _ => (chroma, 0.0, secondary),
    };
    let channel = |component: f64| (component + value - chroma).round() as u8;
    UiColor::new(channel(red), channel(green), channel(blue))
}

/// Thinking duration with one decimal, always in seconds: `0.4s`, `94.3s`.
fn format_thinking_elapsed(elapsed: std::time::Duration) -> String {
    format!("{:.1}s", elapsed.as_secs_f64())
}

pub(in crate::tui) fn render_reasoning(
    kind: ReasoningKind,
    content: &str,
    width: usize,
) -> Vec<String> {
    const STYLE: &str = "\x1b[2;3;38;2;148;148;158m";
    let continuation = format!("\x1b[0m{STYLE}");
    let style = |line: String| format!("{STYLE}{}\x1b[0m", line.replace("\x1b[0m", &continuation));
    match kind {
        ReasoningKind::Summary => {
            let mut lines = Vec::new();
            let mut previous_was_title = false;
            for summary in reasoning_summary_parts(content) {
                let is_title = summary
                    .strip_prefix("**")
                    .and_then(|text| text.strip_suffix("**"))
                    .is_some_and(|text| !text.is_empty() && !text.contains("**"));
                if is_title && !previous_was_title && !lines.is_empty() {
                    lines.push(String::new());
                }
                lines.extend(markdown::render(&summary, width).into_iter().map(style));
                previous_was_title = is_title;
            }
            lines
        }
        ReasoningKind::Full => markdown::render(content.trim(), width)
            .into_iter()
            .map(style)
            .collect(),
    }
}

fn reasoning_summary_parts(content: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_colors_are_distinct_and_readable_for_palette_and_custom_accents() {
        for name in [
            "white", "gray", "red", "orange", "yellow", "green", "cyan", "blue", "purple", "pink",
            "#000000", "#ffffff", "#123456", "#ff0000",
        ] {
            let accent = UiColor::parse(name).unwrap();
            let color = thinking_color(accent);
            assert!(
                super::super::perceptual_color_distance(accent, color) > 2_000,
                "{name}: {color:?}"
            );
            assert!((180..=205).contains(&color.red.max(color.green).max(color.blue)));
            for (streaming, expanded) in
                [(true, false), (true, true), (false, false), (false, true)]
            {
                let lines = render_thinking_tag(None, streaming, expanded, 0, accent, 80);
                assert!(lines[0].starts_with(&foreground_color(color)));
            }
        }
        let neutral = thinking_color(UiColor::WHITE);
        assert!(neutral.blue > neutral.red + 50);
        assert_ne!(
            thinking_color(UiColor::parse("red").unwrap()),
            thinking_color(UiColor::parse("blue").unwrap())
        );
    }
}
