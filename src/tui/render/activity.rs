//! One-row activity strips for background terminals and subagents above the composer.

use std::time::{Duration, Instant};

use crate::background::BackgroundSnapshot;
use crate::config::UiColor;
use crate::subagent::SubagentSnapshot;

use super::{foreground_color, markdown, perceptual_color_distance, status_style};

const CYAN: UiColor = UiColor::new(116, 199, 213);
const AMBER: UiColor = UiColor::new(232, 202, 118);

/// Singular and plural labels from widest to narrowest terminal.
struct Nouns {
    wide: (&'static str, &'static str),
    medium: (&'static str, &'static str),
    narrow: &'static str,
}

const TERMINALS: Nouns = Nouns {
    wide: ("background terminal", "background terminals"),
    medium: ("terminal", "terminals"),
    narrow: "bg",
};

const SUBAGENTS: Nouns = Nouns {
    wide: ("subagent", "subagents"),
    medium: ("subagent", "subagents"),
    narrow: "sa",
};

struct Strip {
    nouns: &'static Nouns,
    command: &'static str,
    count: usize,
    name: String,
    status: String,
    elapsed: Duration,
}

/// Subagent and background-terminal rows, stacked in that order. Rows share
/// label and command columns so their separators, statuses, and actions align.
pub(super) fn render_activity_rows(
    subagents: &[SubagentSnapshot],
    background: &[BackgroundSnapshot],
    width: usize,
    accent: UiColor,
) -> Vec<String> {
    let strips = [subagent_strip(subagents), background_strip(background)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let columns = Columns {
        label: strips
            .iter()
            .map(|strip| markdown::visible_width(&strip_label(strip, width)))
            .max()
            .unwrap_or(0),
        command: strips
            .iter()
            .map(|strip| strip.command.len())
            .max()
            .unwrap_or(0),
        status: strips
            .iter()
            .map(|strip| status_text(strip).chars().count())
            .max()
            .unwrap_or(0),
    };
    strips
        .iter()
        .map(|strip| render_strip(strip, width, accent, columns))
        .collect()
}

fn background_strip(snapshots: &[BackgroundSnapshot]) -> Option<Strip> {
    let mut active = snapshots
        .iter()
        .filter(|snapshot| snapshot.status.is_active());
    let latest = active.next_back()?;
    Some(Strip {
        nouns: &TERMINALS,
        command: "/ps",
        count: active.count() + 1,
        name: latest.display_name().to_string(),
        status: latest.status.label().to_string(),
        elapsed: latest.elapsed(Instant::now()),
    })
}

/// Lists every active child so the row shows exactly which subagents still work.
fn subagent_strip(snapshots: &[SubagentSnapshot]) -> Option<Strip> {
    let active = snapshots
        .iter()
        .filter(|snapshot| snapshot.status.is_active())
        .collect::<Vec<_>>();
    let now = Instant::now();
    let status = match active.as_slice() {
        [] => return None,
        [only] => only.status.label(),
        _ => "running",
    };
    Some(Strip {
        nouns: &SUBAGENTS,
        command: "/subagents",
        count: active.len(),
        name: active
            .iter()
            .map(|snapshot| snapshot.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        status: status.to_string(),
        elapsed: active
            .iter()
            .map(|snapshot| snapshot.elapsed(now))
            .max()
            .unwrap_or_default(),
    })
}

#[derive(Clone, Copy)]
struct Columns {
    label: usize,
    command: usize,
    status: usize,
}

fn strip_label(strip: &Strip, width: usize) -> String {
    let pick = |(singular, plural): (&'static str, &'static str)| {
        if strip.count == 1 { singular } else { plural }
    };
    let noun = if width >= 80 {
        pick(strip.nouns.wide)
    } else if width >= 42 {
        pick(strip.nouns.medium)
    } else {
        strip.nouns.narrow
    };
    format!(" ● {} {noun}", strip.count)
}

fn render_strip(strip: &Strip, width: usize, accent: UiColor, columns: Columns) -> String {
    let color = activity_color(accent);
    let ink = foreground_color(color);
    let muted = status_style(color);
    let count = strip.count;
    if width < 18 {
        return markdown::fit_width(
            &format!(
                " {ink}{count} {}  {}\x1b[0m",
                strip.nouns.narrow, strip.command
            ),
            width,
        );
    }
    let label = markdown::fit_width(&strip_label(strip, width), columns.label);
    let command_width = columns.command;
    let action = if width >= 42 {
        format!(" {:<command_width$}  view ", strip.command)
    } else {
        format!(" {:<command_width$} ", strip.command)
    };
    let room = width.saturating_sub(markdown::visible_width(&label) + action.len());
    let detail = if room >= 12 {
        // Process names, commands, and subagent names are untrusted terminal content.
        let name = markdown::strip_ansi(&strip.name)
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect::<String>();
        let status = format!("{:>width$}", status_text(strip), width = columns.status);
        let name_width = room.saturating_sub(columns.status + 8);
        if name_width >= 8 {
            format!("  │  {}  {status} ", ellipsize(&name, name_width))
        } else {
            format!("{:>room$}", format!("{status} "))
        }
    } else {
        String::new()
    };
    markdown::fit_width(
        &format!(
            "{ink}\x1b[1m{label}\x1b[22m{muted}{}{ink}\x1b[1m{action}\x1b[0m",
            markdown::fit_width(&detail, room),
        ),
        width,
    )
}

fn status_text(strip: &Strip) -> String {
    let seconds = strip.elapsed.as_secs();
    let elapsed = if seconds >= 60 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    };
    format!("{} · {elapsed}", strip.status)
}

fn ellipsize(text: &str, width: usize) -> String {
    if markdown::visible_width(text) <= width {
        markdown::fit_width(text, width)
    } else {
        format!("{}…", markdown::fit_width(text, width.saturating_sub(1)))
    }
}

fn activity_color(accent: UiColor) -> UiColor {
    if perceptual_color_distance(accent, CYAN) >= perceptual_color_distance(accent, AMBER) {
        CYAN
    } else {
        AMBER
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background::{BackgroundId, BackgroundStatus};
    use crate::subagent::{SubagentId, SubagentStatus};

    fn process(number: u64, name: &str, status: BackgroundStatus) -> BackgroundSnapshot {
        BackgroundSnapshot {
            id: BackgroundId::new(number),
            pid: Some(42),
            command: "cargo test".into(),
            name: Some(name.into()),
            cwd: "/tmp".into(),
            timeout: None,
            status,
            started_at: Instant::now() - Duration::from_secs(72),
            settled_at: None,
        }
    }

    fn background_row(snapshots: &[BackgroundSnapshot], width: usize) -> Option<String> {
        render_activity_rows(&[], snapshots, width, UiColor::WHITE).pop()
    }

    fn subagent_row(snapshots: &[SubagentSnapshot], width: usize) -> Option<String> {
        render_activity_rows(snapshots, &[], width, UiColor::WHITE).pop()
    }

    #[test]
    fn stacked_rows_share_separator_status_and_action_columns() {
        let children = [
            child(1, "long-sleep-agent-1", SubagentStatus::Running, 8),
            child(2, "long-sleep-agent-2", SubagentStatus::Running, 7),
        ];
        let processes = [process(
            1,
            "long-background-sleep",
            BackgroundStatus::Running,
        )];
        for width in [60, 100, 190] {
            let rows = render_activity_rows(&children, &processes, width, UiColor::WHITE)
                .iter()
                .map(|row| markdown::strip_ansi(row))
                .collect::<Vec<_>>();
            assert_eq!(rows.len(), 2);
            assert!(rows[0].contains("subagent") && rows[1].contains("terminal"));
            let column = |row: &str, needle: &str| {
                row.find(needle)
                    .map(|index| row[..index].chars().count())
                    .unwrap_or_else(|| panic!("{needle:?} missing in {row:?}"))
            };
            assert_eq!(
                rows[0].contains('│'),
                rows[1].contains('│'),
                "rows show names together or not at all: {rows:#?}"
            );
            assert_eq!(rows[0].contains('│'), width >= 100, "{rows:#?}");
            if width >= 100 {
                assert_eq!(column(&rows[0], "│"), column(&rows[1], "│"), "{rows:#?}");
            }
            assert_eq!(column(&rows[0], "view"), column(&rows[1], "view"));
            assert_eq!(column(&rows[0], "/subagents"), column(&rows[1], "/ps"));
            assert_eq!(
                column(&rows[0], "s  /subagents"),
                column(&rows[1], "s  /ps"),
                "statuses end in the same column"
            );
            for row in &rows {
                assert_eq!(markdown::visible_width(row), width);
            }
        }
        let alone = background_row(&processes, 100).unwrap();
        assert!(markdown::strip_ansi(&alone).ends_with(" /ps  view "));
    }

    fn child(number: u64, name: &str, status: SubagentStatus, age: u64) -> SubagentSnapshot {
        let mut snapshot = SubagentSnapshot::new(
            SubagentId::new(number),
            name.into(),
            "default".into(),
            String::new(),
            "test:model".into(),
            100,
        );
        snapshot.status = status;
        snapshot.started_at = Some(Instant::now() - Duration::from_secs(age));
        snapshot
    }

    #[test]
    fn activity_strip_shows_latest_active_process_and_right_aligned_action() {
        let snapshots = [
            process(1, "dev server", BackgroundStatus::Running),
            process(2, "test suite", BackgroundStatus::Stopping),
            process(3, "finished", BackgroundStatus::Exited(0)),
        ];
        let line = background_row(&snapshots, 100).unwrap();
        let plain = markdown::strip_ansi(&line);
        assert!(plain.contains("● 2 background terminals"));
        assert!(plain.contains("test suite"));
        assert!(plain.contains("stopping · 1m12s"));
        assert!(plain.ends_with(" /ps  view "));
        assert!(!plain.contains("finished"));
        assert_eq!(markdown::visible_width(&line), 100);
        assert!(
            !line.contains("\x1b[48;"),
            "the activity row must leave the terminal background unchanged"
        );
    }

    #[test]
    fn activity_strip_is_bounded_and_sanitizes_process_labels() {
        let snapshots = [process(
            1,
            "\x1b[31m界\n\t\x07long name".repeat(20).as_str(),
            BackgroundStatus::Running,
        )];
        for width in 1..=190 {
            let line = background_row(&snapshots, width).unwrap();
            assert_eq!(markdown::visible_width(&line), width, "width {width}");
            assert!(!line.contains("\x1b[31m"));
            assert!(!line.contains(['\n', '\t', '\x07']));
            if width >= 18 {
                assert!(markdown::strip_ansi(&line).contains("/ps"));
            }
        }
        let wide = background_row(&snapshots, 100).unwrap();
        assert!(wide.contains('…'));
    }

    #[test]
    fn activity_strip_disappears_without_live_processes() {
        assert!(background_row(&[], 80).is_none());
        let snapshots = [process(1, "done", BackgroundStatus::Exited(0))];
        assert!(background_row(&snapshots, 80).is_none());
    }

    #[test]
    fn subagent_strip_lists_only_children_still_working() {
        let snapshots = [
            child(1, "sleep-60", SubagentStatus::Running, 46),
            child(2, "sleep-30", SubagentStatus::Done, 30),
            child(3, "scanner", SubagentStatus::Starting, 5),
        ];
        let line = subagent_row(&snapshots, 100).unwrap();
        let plain = markdown::strip_ansi(&line);
        assert!(plain.contains("● 2 subagents"), "{plain}");
        assert!(plain.contains("│  sleep-60, scanner  "), "{plain}");
        assert!(plain.contains("running · 46s"), "{plain}");
        assert!(!plain.contains("sleep-30"));
        assert!(plain.ends_with(" /subagents  view "));
        assert_eq!(markdown::visible_width(&line), 100);

        let single = subagent_row(&snapshots[..2], 100).unwrap();
        let single = markdown::strip_ansi(&single);
        assert!(single.contains("● 1 subagent  │  sleep-60  "), "{single}");
        assert!(single.contains("running · 46s"), "{single}");
        assert!(subagent_row(&snapshots[1..2], 100).is_none());
    }

    #[test]
    fn subagent_strip_is_bounded_and_sanitizes_names() {
        let snapshots = [child(
            1,
            "\x1b[31mname\n\x07".repeat(20).as_str(),
            SubagentStatus::Running,
            3,
        )];
        for width in 1..=190 {
            let line = subagent_row(&snapshots, width).unwrap();
            assert_eq!(markdown::visible_width(&line), width, "width {width}");
            assert!(!line.contains("\x1b[31m"));
            assert!(!line.contains(['\n', '\x07']));
            if width >= 24 {
                assert!(markdown::strip_ansi(&line).contains("/subagents"));
            }
        }
    }

    #[test]
    fn activity_color_stays_distinct_from_the_accent() {
        assert_eq!(activity_color(UiColor::new(110, 195, 210)), AMBER);
        assert_eq!(activity_color(UiColor::parse("cyan").unwrap()), AMBER);
        assert_eq!(activity_color(UiColor::parse("yellow").unwrap()), CYAN);
    }
}
