//! Reuse rendered transcript entries across frames and index their row ranges.
//!
//! This module decides when to render again, not how an entry looks. Entry
//! presentation lives in `entries`; visible-window composition lives in `transcript`.

use std::sync::Arc;

use crate::config::UiColor;

use super::ImageSupport;
use super::entries::{RenderedEntry, entry_default_expanded, render_entry};

#[derive(Debug, Clone, Copy)]
pub(super) struct RenderSettings {
    pub(super) width: usize,
    pub(super) tools_expanded: bool,
    pub(super) hide_reasoning: bool,
    pub(super) image_support: ImageSupport,
    pub(super) accent_color: UiColor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CacheSlot {
    width: usize,
    tools_expanded: bool,
    hide_reasoning: bool,
    image_support: ImageSupport,
    accent_color: UiColor,
    entries: Vec<Option<RenderedEntry>>,
    /// Absolute starting line for every entry, plus one total-height sentinel.
    entry_starts: Vec<usize>,
    frozen_count: usize,
}

/// Owner of one rendered transcript row: which entry it belongs to and
/// whether it is the entry's first line (its header/tag row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) struct LineOwner {
    pub(in crate::tui) entry: usize,
    pub(in crate::tui) first: bool,
}

impl CacheSlot {
    fn new(settings: RenderSettings) -> Self {
        Self {
            width: settings.width,
            tools_expanded: settings.tools_expanded,
            hide_reasoning: settings.hide_reasoning,
            image_support: settings.image_support,
            accent_color: settings.accent_color,
            entries: Vec::new(),
            entry_starts: vec![0],
            frozen_count: 0,
        }
    }

    fn matches(&self, settings: RenderSettings) -> bool {
        self.width == settings.width
            && self.tools_expanded == settings.tools_expanded
            && self.hide_reasoning == settings.hide_reasoning
            && self.image_support == settings.image_support
            && self.accent_color == settings.accent_color
    }

    fn update(
        &mut self,
        transcript: &super::super::transcript::Transcript,
        tools_expanded: bool,
        hide_reasoning: bool,
        labels: &[(&str, &str)],
        spinner_tick: usize,
    ) {
        let entries = transcript.entries();
        if entries.len() < self.frozen_count {
            self.frozen_count = entries.len();
        }

        let mutable_index = transcript
            .streaming_index()
            .or_else(|| transcript.running_tool_index());
        let frozen_boundary = mutable_index.unwrap_or(entries.len()).min(entries.len());

        let mut changed_from = None;
        if self.entries.len() < entries.len() {
            changed_from = Some(self.entries.len());
            self.entries.resize_with(entries.len(), || None);
        } else if self.entries.len() > entries.len() {
            self.entries.truncate(entries.len());
            changed_from = Some(entries.len());
        }

        for (i, entry) in entries
            .iter()
            .enumerate()
            .take(frozen_boundary)
            .skip(self.frozen_count)
        {
            let expanded =
                transcript.entry_expanded(i, entry_default_expanded(entry, tools_expanded));
            self.entries[i] = render_entry(
                entry,
                self.width,
                expanded,
                hide_reasoning,
                self.image_support,
                self.accent_color,
                labels,
                spinner_tick,
            );
            changed_from = Some(changed_from.map_or(i, |changed| changed.min(i)));
        }
        self.frozen_count = frozen_boundary;

        if let Some(idx) = mutable_index
            && idx < entries.len()
        {
            let expanded = transcript
                .entry_expanded(idx, entry_default_expanded(&entries[idx], tools_expanded));
            self.entries[idx] = render_entry(
                &entries[idx],
                self.width,
                expanded,
                hide_reasoning,
                self.image_support,
                self.accent_color,
                labels,
                spinner_tick,
            );
            changed_from = Some(changed_from.map_or(idx, |changed| changed.min(idx)));
        }

        if let Some(start) = changed_from {
            self.entry_starts.resize(self.entries.len() + 1, 0);
            if start == 0 {
                self.entry_starts[0] = 0;
            }
            for index in start..self.entries.len() {
                let height = self.entries[index]
                    .as_ref()
                    .map_or(0, |entry| entry.lines.len() + 1);
                self.entry_starts[index + 1] = self.entry_starts[index] + height;
            }
        }
    }

    pub(super) fn total_lines(&self) -> usize {
        self.entry_starts.last().copied().unwrap_or(0)
    }

    pub(super) fn entry_range(&self, index: usize) -> Option<std::ops::Range<usize>> {
        let start = *self.entry_starts.get(index)?;
        let end = *self.entry_starts.get(index + 1)?;
        (start < end).then_some(start..end)
    }

    pub(super) fn line_at(&self, row: usize) -> Option<(String, Option<LineOwner>)> {
        if row >= self.total_lines() {
            return None;
        }
        let index = self
            .entry_starts
            .partition_point(|start| *start <= row)
            .saturating_sub(1)
            .min(self.entries.len().saturating_sub(1));
        let offset = row.saturating_sub(self.entry_starts[index]);
        let entry = self.entries.get(index)?.as_ref()?;
        if offset < entry.lines.len() {
            Some((
                entry.lines[offset].clone(),
                Some(LineOwner {
                    entry: index,
                    first: offset == 0,
                }),
            ))
        } else {
            Some((String::new(), None))
        }
    }

    pub(super) fn images_in(
        &self,
        visible: std::ops::Range<usize>,
        screen_offset: usize,
    ) -> Vec<super::FrameImage> {
        let mut images = Vec::new();
        if visible.is_empty() {
            return images;
        }
        // Entries are laid out in `entry_starts` order, so binary-search the
        // window instead of scanning the whole transcript every frame.
        let first = self
            .entry_starts
            .partition_point(|start| *start <= visible.start)
            .saturating_sub(1);
        let end = self
            .entry_starts
            .partition_point(|start| *start < visible.end)
            .min(self.entries.len());
        for index in first..end {
            let Some(entry) = &self.entries[index] else {
                continue;
            };
            let entry_start = self.entry_starts[index];
            for image in &entry.images {
                let start = entry_start + image.start_line;
                let end = start + image.rows;
                if start < visible.start || end > visible.end {
                    continue;
                }
                images.push(super::FrameImage {
                    key: Arc::as_ptr(&image.content) as usize,
                    row: screen_offset + start - visible.start + 1,
                    column: 2,
                    columns: image.columns,
                    rows: image.rows,
                    content: Arc::clone(&image.content),
                });
            }
        }
        images
    }

    #[cfg(test)]
    fn flattened(&self) -> Vec<String> {
        (0..self.total_lines())
            .filter_map(|row| self.line_at(row).map(|(line, _)| line))
            .collect()
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(in crate::tui) struct RenderCache {
    slots: [Option<CacheSlot>; 2],
}

impl RenderCache {
    pub(in crate::tui) fn invalidate(&mut self) {
        self.slots = Default::default();
    }

    pub(super) fn get_or_render_slot<'a>(
        &'a mut self,
        transcript: &super::super::transcript::Transcript,
        settings: RenderSettings,
        labels: &[(&str, &str)],
        spinner_tick: usize,
    ) -> &'a CacheSlot {
        let slot_idx = self
            .slots
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|slot| slot.matches(settings)));

        let idx = match slot_idx {
            Some(i) => i,
            None => {
                let empty_idx = self.slots.iter().position(|s| s.is_none()).unwrap_or(1);
                self.slots[empty_idx] = Some(CacheSlot::new(settings));
                empty_idx
            }
        };

        let slot = self.slots[idx].as_mut().expect("slot was set above");
        slot.update(
            transcript,
            settings.tools_expanded,
            settings.hide_reasoning,
            labels,
            spinner_tick,
        );
        slot
    }

    #[cfg(test)]
    pub(in crate::tui) fn get_or_render(
        &mut self,
        transcript: &super::super::transcript::Transcript,
        tools_expanded: bool,
        hide_reasoning: bool,
        accent_color: UiColor,
        width: usize,
    ) -> Vec<String> {
        self.get_or_render_slot(
            transcript,
            RenderSettings {
                width,
                tools_expanded,
                hide_reasoning,
                image_support: ImageSupport::None,
                accent_color,
            },
            &[],
            0,
        )
        .flattened()
    }
}

#[cfg(test)]
mod tests {
    use super::super::entries::EntryImage;
    use super::*;
    use crate::provider::ImageContent;

    fn preview(rows: usize) -> EntryImage {
        EntryImage {
            start_line: 0,
            columns: 4,
            rows,
            content: Arc::new(ImageContent {
                media_type: "image/png".into(),
                data: String::new(),
            }),
        }
    }

    fn slot_with_images() -> CacheSlot {
        let mut slot = CacheSlot::new(RenderSettings {
            width: 80,
            tools_expanded: false,
            hide_reasoning: false,
            image_support: ImageSupport::None,
            accent_color: UiColor::WHITE,
        });
        slot.entries = vec![
            Some(RenderedEntry {
                lines: vec![String::new(); 3],
                images: vec![preview(2)],
            }),
            None,
            Some(RenderedEntry {
                lines: vec![String::new(); 5],
                images: vec![preview(3)],
            }),
            Some(RenderedEntry {
                lines: vec![String::new(); 2],
                images: vec![preview(1)],
            }),
        ];
        let mut starts = vec![0];
        for entry in &slot.entries {
            let height = entry.as_ref().map_or(0, |entry| entry.lines.len() + 1);
            let last = *starts.last().expect("start");
            starts.push(last + height);
        }
        slot.entry_starts = starts;
        slot
    }

    #[test]
    fn images_are_collected_only_from_entries_intersecting_the_window() {
        let slot = slot_with_images();
        assert_eq!(slot.entry_starts, [0, 4, 4, 10, 13]);

        let all = slot.images_in(0..13, 0);
        assert_eq!(all.len(), 3);

        let middle = slot.images_in(4..10, 2);
        assert_eq!(middle.len(), 1, "only the second rendered entry intersects");
        assert_eq!(middle[0].row, 3);
        assert_eq!(middle[0].rows, 3);

        let first = slot.images_in(0..4, 0);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].row, 1);

        assert!(slot.images_in(13..20, 0).is_empty());
        assert!(slot.images_in(6..6, 0).is_empty());
    }
}
