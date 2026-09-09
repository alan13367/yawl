//! Edit-diff calculation for file-edit tool calls.
//!
//! Generic tool-card rendering stays in the tool-view facade; this child
//! owns the line-diff algorithm with context elision.

use super::{Tone, ToolLine};

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffKind {
    Equal,
    Added,
    Removed,
}

struct DiffLine<'a> {
    text: &'a str,
    kind: DiffKind,
}

pub(in crate::tui) fn edit_diff(old: &str, new: &str) -> Vec<ToolLine> {
    const CONTEXT: usize = 2;
    let old_lines = old.lines().collect::<Vec<_>>();
    let new_lines = new.lines().collect::<Vec<_>>();
    let diff = line_diff(&old_lines, &new_lines);
    let mut rendered = vec![ToolLine::new(
        format!("@@ -{} +{} @@", old_lines.len(), new_lines.len()),
        Tone::Muted,
    )];
    let mut omitted = false;
    for (index, line) in diff.iter().enumerate() {
        let nearby_change = line.kind != DiffKind::Equal
            || diff[index.saturating_sub(CONTEXT)..(index + CONTEXT + 1).min(diff.len())]
                .iter()
                .any(|candidate| candidate.kind != DiffKind::Equal);
        if !nearby_change {
            if !omitted {
                rendered.push(ToolLine::new("  … unchanged lines …", Tone::Muted));
                omitted = true;
            }
            continue;
        }
        omitted = false;
        let (prefix, tone) = match line.kind {
            DiffKind::Equal => ("  ", Tone::Output),
            DiffKind::Added => ("+ ", Tone::Added),
            DiffKind::Removed => ("- ", Tone::Removed),
        };
        rendered.push(ToolLine::new(format!("{prefix}{}", line.text), tone));
    }
    rendered
}

fn line_diff<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<DiffLine<'a>> {
    const MAX_CELLS: usize = 250_000;
    if old.len().saturating_mul(new.len()) > MAX_CELLS {
        return coarse_line_diff(old, new);
    }
    let columns = new.len() + 1;
    let mut lcs = vec![0_u32; (old.len() + 1) * columns];
    for old_index in (0..old.len()).rev() {
        for new_index in (0..new.len()).rev() {
            let here = old_index * columns + new_index;
            lcs[here] = if old[old_index] == new[new_index] {
                lcs[(old_index + 1) * columns + new_index + 1] + 1
            } else {
                lcs[(old_index + 1) * columns + new_index]
                    .max(lcs[old_index * columns + new_index + 1])
            };
        }
    }
    let mut diff = Vec::with_capacity(old.len() + new.len());
    let (mut old_index, mut new_index) = (0, 0);
    while old_index < old.len() || new_index < new.len() {
        if old_index < old.len() && new_index < new.len() && old[old_index] == new[new_index] {
            diff.push(DiffLine {
                text: old[old_index],
                kind: DiffKind::Equal,
            });
            old_index += 1;
            new_index += 1;
        } else if old_index < old.len()
            && (new_index == new.len()
                || lcs[(old_index + 1) * columns + new_index]
                    >= lcs[old_index * columns + new_index + 1])
        {
            diff.push(DiffLine {
                text: old[old_index],
                kind: DiffKind::Removed,
            });
            old_index += 1;
        } else {
            diff.push(DiffLine {
                text: new[new_index],
                kind: DiffKind::Added,
            });
            new_index += 1;
        }
    }
    diff
}

fn coarse_line_diff<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<DiffLine<'a>> {
    let prefix = old
        .iter()
        .zip(new)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    old[..prefix]
        .iter()
        .map(|text| DiffLine {
            text,
            kind: DiffKind::Equal,
        })
        .chain(old[prefix..old.len() - suffix].iter().map(|text| DiffLine {
            text,
            kind: DiffKind::Removed,
        }))
        .chain(new[prefix..new.len() - suffix].iter().map(|text| DiffLine {
            text,
            kind: DiffKind::Added,
        }))
        .chain(old[old.len() - suffix..].iter().map(|text| DiffLine {
            text,
            kind: DiffKind::Equal,
        }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_diff_preserves_context_and_marks_only_changed_lines() {
        let rendered = edit_diff("before\nold\nafter", "before\nnew\nafter");
        let plain = rendered
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(plain.contains("  before"));
        assert!(plain.contains("- old"));
        assert!(plain.contains("+ new"));
        assert!(plain.contains("  after"));
    }

    #[test]
    fn edit_diff_elides_distant_unchanged_lines() {
        let old = (0..20).map(|line| line.to_string()).collect::<Vec<_>>();
        let mut new = old.clone();
        new[10] = "changed".into();
        let rendered = edit_diff(&old.join("\n"), &new.join("\n"));

        assert!(rendered.iter().any(|line| line.text.contains("unchanged")));
    }
}
