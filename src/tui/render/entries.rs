//! Render individual transcript entries and user, queued, and steer panels.
//!
//! Returns lines and entry-relative image placements. Cache reuse and screen
//! positioning belong to `cache` and `transcript`, respectively.

use std::sync::Arc;

use base64::Engine as _;

use crate::config::UiColor;
use crate::provider::ImageContent;

use super::super::transcript::Entry;
use super::super::{USER_BACKGROUND, USER_TEXT, markdown, tool_view};
use super::reasoning::{render_reasoning, render_thinking_tag};
use super::{ImageSupport, foreground_color};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EntryImage {
    pub(super) start_line: usize,
    pub(super) columns: usize,
    pub(super) rows: usize,
    pub(super) content: Arc<ImageContent>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct RenderedEntry {
    pub(super) lines: Vec<String>,
    pub(super) images: Vec<EntryImage>,
}

pub(in crate::tui) fn entry_default_expanded(entry: &Entry, tools_expanded: bool) -> bool {
    match entry {
        Entry::Reasoning { .. } => false,
        Entry::Tool { .. } | Entry::Diff { .. } => tools_expanded,
        _ => true,
    }
}

#[expect(clippy::too_many_arguments)]
pub(super) fn render_entry(
    entry: &Entry,
    width: usize,
    expanded: bool,
    hide_reasoning: bool,
    image_support: ImageSupport,
    accent_color: UiColor,
    labels: &[(&str, &str)],
    spinner_tick: usize,
) -> Option<RenderedEntry> {
    let lines = match entry {
        Entry::User(content) if !expanded => render_collapsed("Prompt", content, width),
        Entry::User(content) => render_user_panel(content, width),
        Entry::Steer(content) if !expanded => render_collapsed("Steer", content, width),
        Entry::Steer(content) => render_user_panel(content, width),
        Entry::Assistant(content) => {
            if content.trim().is_empty() {
                return None;
            } else if !expanded {
                render_collapsed("Reply", content, width)
            } else {
                markdown::render(content.trim(), width)
            }
        }
        Entry::Reasoning {
            kind,
            content,
            started,
            duration,
        } => {
            if hide_reasoning || content.trim().is_empty() {
                return None;
            }
            let mut lines = render_thinking_tag(
                *duration,
                started.is_some(),
                expanded,
                spinner_tick,
                accent_color,
                width,
            );
            if expanded {
                lines.push(String::new());
                lines.extend(render_reasoning(*kind, content, width));
            }
            lines
        }
        Entry::Tool {
            name,
            args,
            output,
            images,
            is_error,
            running,
            started,
        } => {
            let mut lines = tool_view::render_labeled(
                name,
                args,
                output,
                *is_error,
                *running,
                started.map(|started| started.elapsed()),
                labels,
                width,
                expanded,
            );
            let previews = image_previews(images, image_support, width, lines.len());
            let reserved_rows = previews.iter().map(|image| image.rows).sum();
            lines.extend(std::iter::repeat_n(String::new(), reserved_rows));
            return Some(RenderedEntry {
                lines,
                images: previews,
            });
        }
        Entry::Notice(content) if !expanded => render_collapsed("Notice", content, width),
        Entry::Notice(content) => {
            let mut lines = vec![yawl_label(accent_color)];
            lines.extend(markdown::render(content, width));
            lines
        }
        Entry::Diff { path, lines } => tool_view::render_diff_card(path, lines, width, expanded),
        Entry::SubagentResult {
            id,
            name,
            status,
            content,
        } if !expanded => render_collapsed(name, content, width),
        Entry::SubagentResult {
            id,
            name,
            status,
            content,
        } => render_subagent_result(id, name, status, content, width, true),
    };
    Some(RenderedEntry {
        lines,
        images: Vec::new(),
    })
}

fn image_previews(
    images: &[Arc<ImageContent>],
    support: ImageSupport,
    width: usize,
    mut start_line: usize,
) -> Vec<EntryImage> {
    let columns = width.saturating_sub(4).clamp(1, 100);
    images
        .iter()
        .filter(|image| support.accepts(&image.media_type))
        .filter_map(|image| {
            let max_encoded_bytes = crate::image::MAX_IMAGE_BYTES.div_ceil(3) * 4;
            if image.data.len() > max_encoded_bytes {
                return None;
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&image.data)
                .ok()?;
            if bytes.len() > crate::image::MAX_IMAGE_BYTES
                || crate::image::media_type(&bytes) != Some(image.media_type.as_str())
            {
                return None;
            }
            let (pixel_width, pixel_height) = crate::image::dimensions(&bytes)?;
            let numerator = u64::from(pixel_height).saturating_mul(columns as u64);
            let denominator = u64::from(pixel_width).saturating_mul(2).max(1);
            let rows = usize::try_from(numerator.div_ceil(denominator))
                .unwrap_or(12)
                .clamp(3, 12);
            let preview = EntryImage {
                start_line,
                columns,
                rows,
                content: Arc::clone(image),
            };
            start_line += rows;
            Some(preview)
        })
        .collect()
}

fn render_collapsed(label: &str, content: &str, width: usize) -> Vec<String> {
    let preview = content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let preview = markdown::render(preview, width)
        .into_iter()
        .next()
        .unwrap_or_default();
    let label = crate::subagent::sanitize_preview(label, 128);
    vec![markdown::fit_width(
        &format!("\x1b[2m▸ {label}\x1b[0m  {preview}"),
        width,
    )]
}

#[cfg(test)]
pub(in crate::tui) fn render_entries(
    entries: &[Entry],
    width: usize,
    tools_expanded: bool,
    hide_reasoning: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    for entry in entries {
        if let Some(rendered) = render_entry(
            entry,
            width,
            entry_default_expanded(entry, tools_expanded),
            hide_reasoning,
            ImageSupport::None,
            UiColor::WHITE,
            &[],
            0,
        ) {
            lines.extend(rendered.lines);
            lines.push(String::new());
        }
    }
    lines
}

#[cfg(test)]
pub(in crate::tui) fn render_expanded(entry: &Entry, width: usize) -> Vec<String> {
    render_entry(
        entry,
        width,
        true,
        false,
        ImageSupport::None,
        UiColor::WHITE,
        &[],
        0,
    )
    .map(|rendered| rendered.lines)
    .unwrap_or_default()
}

fn render_subagent_result(
    id: &str,
    name: &str,
    status: &str,
    content: &str,
    width: usize,
    expanded: bool,
) -> Vec<String> {
    let id = crate::subagent::sanitize_preview(id, 256);
    let name = crate::subagent::sanitize_preview(name, 1024);
    let status = crate::subagent::sanitize_preview(status, 64);
    let color = if status == "failed" || status == "interrupted" {
        "\x1b[1;31m"
    } else {
        "\x1b[1;36m"
    };
    let mut lines = vec![markdown::fit_width(
        &format!("{color}Subagent {id} [{status}] {name}\x1b[0m"),
        width,
    )];
    let rendered = markdown::render(content, width);
    if expanded || rendered.len() <= 8 {
        lines.extend(rendered);
    } else {
        lines.extend(rendered.into_iter().take(8));
        lines.push(markdown::fit_width(
            "\x1b[2m… Ctrl+O to expand\x1b[0m",
            width,
        ));
    }
    lines
}

pub(in crate::tui) fn render_user_panel(content: &str, width: usize) -> Vec<String> {
    let panel_width = width.max(1);
    let horizontal_padding = usize::from(panel_width >= 3);
    let content_width = panel_width.saturating_sub(horizontal_padding * 2).max(1);
    let blank = format!("{USER_BACKGROUND}{}\x1b[0m", " ".repeat(panel_width));
    let continuation = format!("\x1b[0m{USER_BACKGROUND}{USER_TEXT}");
    let mut lines = Vec::new();
    lines.push(blank.clone());
    lines.extend(
        markdown::render(content, content_width)
            .into_iter()
            .map(|line| {
                let fitted =
                    markdown::fit_width(&line, content_width).replace("\x1b[0m", &continuation);
                format!(
                    "{USER_BACKGROUND}{USER_TEXT}{}{fitted}{}\x1b[0m",
                    " ".repeat(horizontal_padding),
                    " ".repeat(horizontal_padding)
                )
            }),
    );
    lines.push(blank);
    lines
}

pub(in crate::tui) fn render_queued_panel(
    content: &str,
    position: usize,
    width: usize,
) -> Vec<String> {
    let mut lines = vec![markdown::fit_width(
        &format!("\x1b[2;33mQueued {position} · waiting for the active response\x1b[0m"),
        width,
    )];
    lines.extend(render_user_panel(content, width));
    lines.push(String::new());
    lines
}

pub(super) fn render_steer_panel(content: &str, position: usize, width: usize) -> Vec<String> {
    let mut lines = vec![markdown::fit_width(
        &format!("\x1b[2;36mSteer {position} · waiting for a safe boundary\x1b[0m"),
        width,
    )];
    lines.extend(render_user_panel(content, width));
    lines.push(String::new());
    lines
}

fn yawl_label(accent: UiColor) -> String {
    format!("{}\x1b[1mYawl\x1b[0m", foreground_color(accent))
}
