//! Shared compact panel and table primitives for full-screen dashboards.

use crate::config::UiColor;

use super::markdown;
use super::render::{foreground_color, selected_row, selection_style, status_style};

const BASE_PANEL_CHROME_ROWS: usize = 7;
const MAX_PANEL_WIDTH: usize = 116;

#[derive(Clone, Copy)]
pub(super) enum Alignment {
    Left,
    Right,
}

pub(super) struct Column {
    header: &'static str,
    width: usize,
    alignment: Alignment,
}

impl Column {
    pub(super) const fn new(header: &'static str, width: usize, alignment: Alignment) -> Self {
        Self {
            header,
            width,
            alignment,
        }
    }
}

pub(super) fn render_header(columns: &[Column], width: usize) -> String {
    render_cells(
        columns,
        &columns
            .iter()
            .map(|column| column.header.to_string())
            .collect::<Vec<_>>(),
        width,
    )
}

pub(super) fn render_row(columns: &[Column], cells: &[String], width: usize) -> String {
    debug_assert_eq!(columns.len(), cells.len());
    render_cells(columns, cells, width)
}

pub(super) fn summary(active: usize, total: usize, active_label: &str) -> String {
    match (active, total) {
        (0, total) => format!("{total} tracked"),
        (active, total) if active == total => format!("{active} {active_label}"),
        (active, total) => format!("{active} {active_label} · {total} tracked"),
    }
}

fn render_cells(columns: &[Column], cells: &[String], width: usize) -> String {
    let line = columns
        .iter()
        .zip(cells)
        .map(|(column, cell)| fit_cell(cell, column.width, column.alignment))
        .collect::<Vec<_>>()
        .join(" ");
    markdown::fit_width(&line, width)
}

fn fit_cell(text: &str, width: usize, alignment: Alignment) -> String {
    if width == 0 {
        return String::new();
    }
    let visible = markdown::visible_width(text);
    if visible > width {
        return if width == 1 {
            "…".into()
        } else {
            format!("{}…", markdown::fit_width(text, width - 1))
        };
    }
    let padding = " ".repeat(width - visible);
    match alignment {
        Alignment::Left => format!("{text}{padding}"),
        Alignment::Right => format!("{padding}{text}"),
    }
}

pub(super) struct PanelRow {
    pub(super) content: String,
    pub(super) selected: bool,
}

pub(super) struct PanelContent<'a> {
    pub(super) title: &'a str,
    pub(super) summary: &'a str,
    pub(super) header: &'a str,
    pub(super) rows: &'a [PanelRow],
    pub(super) hint: &'a str,
    pub(super) accent: UiColor,
    pub(super) selection: UiColor,
}

/// Geometry for a dense dashboard card centered within the alternate screen.
pub(super) struct Panel {
    columns: usize,
    rows: usize,
    width: usize,
    inner_width: usize,
    visible_rows: usize,
    header_spacing: bool,
}

impl Panel {
    pub(super) fn new(columns: usize, rows: usize, item_count: usize) -> Self {
        let columns = columns.max(20);
        let rows = rows.max(8);
        let width = if columns >= 28 {
            columns.saturating_sub(4).min(MAX_PANEL_WIDTH)
        } else {
            columns
        };
        let header_spacing = rows >= 9;
        let chrome_rows = BASE_PANEL_CHROME_ROWS + usize::from(header_spacing);
        let outer_margin = usize::from(rows >= chrome_rows + 5) * 4;
        let max_height = rows.saturating_sub(outer_margin);
        let capacity = max_height.saturating_sub(chrome_rows).max(1);
        let visible_rows = item_count.max(1).min(capacity);
        Self {
            columns,
            rows,
            width,
            inner_width: width.saturating_sub(2),
            visible_rows,
            header_spacing,
        }
    }

    pub(super) const fn inner_width(&self) -> usize {
        self.inner_width
    }

    pub(super) const fn capacity(&self) -> usize {
        self.visible_rows
    }

    pub(super) fn render(&self, content: PanelContent<'_>) -> Vec<String> {
        debug_assert_eq!(content.rows.len(), self.visible_rows);
        let chrome_rows = BASE_PANEL_CHROME_ROWS + usize::from(self.header_spacing);
        let mut panel = Vec::with_capacity(content.rows.len() + chrome_rows);
        panel.push(self.rule('╭', '─', '╮'));
        panel.push(self.bordered(&title_line(
            content.title,
            content.summary,
            self.inner_width,
            content.accent,
        )));
        panel.push(self.rule('├', '─', '┤'));
        panel.push(self.bordered(&format!("\x1b[2m{}\x1b[0m", content.header)));
        if self.header_spacing {
            panel.push(self.bordered(""));
        }
        let selection_style = selection_style(content.selection);
        for row in content.rows {
            let fitted = markdown::fit_width(&row.content, self.inner_width);
            let content = if row.selected {
                selected_row(&fitted, &selection_style)
            } else {
                fitted
            };
            panel.push(self.bordered(&content));
        }
        panel.push(self.rule('├', '─', '┤'));
        panel.push(self.bordered(&format!(
            "{}{}\x1b[0m",
            status_style(content.accent),
            content.hint
        )));
        panel.push(self.rule('╰', '─', '╯'));

        let top = self.rows.saturating_sub(panel.len()) / 2;
        let left = self.columns.saturating_sub(self.width) / 2;
        let mut frame = vec![" ".repeat(self.columns); self.rows];
        for (offset, line) in panel.into_iter().enumerate() {
            frame[top + offset] =
                markdown::fit_width(&format!("{}{line}", " ".repeat(left)), self.columns);
        }
        frame
    }

    fn bordered(&self, content: &str) -> String {
        format!(
            "\x1b[2m│\x1b[0m{}\x1b[2m│\x1b[0m",
            markdown::fit_width(content, self.inner_width)
        )
    }

    fn rule(&self, left: char, fill: char, right: char) -> String {
        format!(
            "\x1b[2m{left}{}{right}\x1b[0m",
            fill.to_string().repeat(self.inner_width)
        )
    }
}

fn title_line(title: &str, summary: &str, width: usize, accent: UiColor) -> String {
    let title_width = markdown::visible_width(title);
    let summary_width = markdown::visible_width(summary);
    let title = format!("{}\x1b[1m{title}\x1b[0m", foreground_color(accent));
    if title_width + summary_width + 1 > width {
        return markdown::fit_width(&title, width);
    }
    format!(
        "{title}{}{}{summary}\x1b[0m",
        " ".repeat(width - title_width - summary_width),
        status_style(accent)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_stays_compact_and_centered_for_one_item() {
        let panel = Panel::new(80, 24, 1);
        let rows = [PanelRow {
            content: "row".into(),
            selected: true,
        }];
        let frame = panel.render(PanelContent {
            title: "Background terminals",
            summary: "1 running",
            header: "HEADER",
            rows: &rows,
            hint: "Esc close",
            accent: UiColor::WHITE,
            selection: UiColor::WHITE,
        });

        assert_eq!(frame.len(), 24);
        assert!(frame.iter().all(|line| markdown::visible_width(line) == 80));
        let occupied = frame
            .iter()
            .enumerate()
            .filter(|(_, line)| !markdown::strip_ansi(line).trim().is_empty())
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert_eq!(occupied.len(), 9);
        assert!(occupied[0] > 0);
        assert!(occupied[8] < 23);
        assert!(markdown::strip_ansi(&frame[occupied[0]]).starts_with("  ╭"));
        let header_index = frame
            .iter()
            .position(|line| markdown::strip_ansi(line).contains("HEADER"))
            .expect("the header should be visible");
        let spacer = markdown::strip_ansi(&frame[header_index + 1]);
        let spacer = spacer.trim().trim_matches('│');
        assert!(spacer.trim().is_empty());
    }

    #[test]
    fn table_cells_share_header_positions_and_truncate_cleanly() {
        let columns = [
            Column::new("STATUS", 8, Alignment::Left),
            Column::new("TIME", 5, Alignment::Right),
        ];
        let header = markdown::strip_ansi(&render_header(&columns, 14));
        let row = markdown::strip_ansi(&render_row(
            &columns,
            &["running forever".into(), "2s".into()],
            14,
        ));

        let right_edge = |line: &str, value: &str| {
            let start = line.find(value).expect("fixture value should be visible");
            markdown::visible_width(&line[..start]) + markdown::visible_width(value)
        };
        assert_eq!(right_edge(&header, "TIME"), right_edge(&row, "2s"));
        assert!(row.contains('…'));
        assert_eq!(markdown::visible_width(&row), 14);
    }
}
