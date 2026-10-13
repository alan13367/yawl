//! Welcome animation rendering.
//!
//! The frame builder coordinates each frame; this child renders the
//! centered startup wordmark, its typing animation, and the project facts
//! shown under it once the hint finishes typing.

use std::path::{Path, PathBuf};

use crate::agent::Agent;
use crate::config::UiColor;

use super::super::markdown;
use super::super::tool_view::home_relative;
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

/// Where Yawl runs and what it loaded, gathered when the view is built.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::tui) struct WelcomeDetails {
    /// Working directory with `~` for the home directory, plus the branch.
    location: String,
    /// Loaded instruction files, skill count, and resumable sessions.
    facts: Vec<String>,
}

impl WelcomeDetails {
    pub(in crate::tui) fn gather(agent: &Agent, skill_count: usize) -> Self {
        let cwd = crate::config::working_dir();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut location = home_relative(&cwd, home.as_deref());
        if let Some(branch) = git_branch(&cwd) {
            location.push_str(" · ");
            location.push_str(&branch);
        }
        let loaded =
            |path: &Path| std::fs::read_to_string(path).is_ok_and(|text| !text.trim().is_empty());
        let mut facts = Vec::new();
        if loaded(&cwd.join("AGENTS.md")) {
            facts.push("AGENTS.md".to_string());
        }
        if loaded(&agent.config().home_dir.join("AGENTS.md")) {
            facts.push("~/.yawl/AGENTS.md".to_string());
        }
        if skill_count > 0 {
            facts.push(count_label(skill_count, "skill"));
        }
        let earlier = crate::session::list(&agent.config().session_dirs(&cwd).project).map_or(
            0,
            |sessions| {
                sessions
                    .iter()
                    .filter(|session| session.id != agent.session_id())
                    .count()
            },
        );
        if earlier > 0 {
            facts.push(format!(
                "{} · /resume",
                count_label(earlier, "earlier session")
            ));
        }
        Self {
            location: printable(&location),
            facts,
        }
    }

    #[cfg(test)]
    pub(in crate::tui) fn new(location: &str, facts: &[&str]) -> Self {
        Self {
            location: location.to_string(),
            facts: facts.iter().map(|fact| fact.to_string()).collect(),
        }
    }

    /// Rows that fit `width`: the location keeps its tail when too long, and
    /// facts pack onto as few rows as fit.
    fn rows(&self, width: usize) -> Vec<String> {
        let mut rows = Vec::new();
        if !self.location.is_empty() && width > 1 {
            rows.push(if markdown::visible_width(&self.location) <= width {
                self.location.clone()
            } else {
                // Columns, not characters: wide glyphs must not push the
                // branch past the row.
                let mut tail = String::new();
                let mut columns = 0;
                for character in self.location.chars().rev() {
                    columns += markdown::visible_width(character.encode_utf8(&mut [0; 4]));
                    if columns > width - 1 {
                        break;
                    }
                    tail.insert(0, character);
                }
                format!("…{tail}")
            });
        }
        let mut row = String::new();
        for fact in &self.facts {
            let joined = if row.is_empty() {
                fact.clone()
            } else {
                format!("{row} · {fact}")
            };
            if markdown::visible_width(&joined) <= width {
                row = joined;
            } else {
                if !row.is_empty() {
                    rows.push(std::mem::take(&mut row));
                }
                if markdown::visible_width(fact) <= width {
                    row = fact.clone();
                }
            }
        }
        if !row.is_empty() {
            rows.push(row);
        }
        rows
    }
}

fn count_label(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

fn printable(text: &str) -> String {
    text.chars()
        .filter(|character| !crate::text::is_unsafe(*character))
        .collect()
}

/// The checked-out branch, or a short commit for a detached `HEAD`, read
/// from the repository files so startup does not wait on a `git` process.
fn git_branch(cwd: &Path) -> Option<String> {
    let dot_git = cwd
        .ancestors()
        .map(|dir| dir.join(".git"))
        .find(|path| path.exists())?;
    let git_dir = if dot_git.is_file() {
        // Worktrees and submodules point at their real Git directory.
        let pointer = std::fs::read_to_string(&dot_git).ok()?;
        let target = PathBuf::from(pointer.strip_prefix("gitdir:")?.trim());
        dot_git.parent()?.join(target)
    } else {
        dot_git
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_string()),
        None => head.get(..7).map(str::to_string),
    }
}

pub(super) fn render_welcome(
    accent: UiColor,
    details: &WelcomeDetails,
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
            content.push(blank.clone());
        } else {
            content.push(center_styled(hint, &status_style(accent), width));
        }
        let rows = details.rows(width);
        if hint == HINT && !rows.is_empty() && height >= content.len() + 1 + rows.len() {
            content.push(blank);
            content.extend(rows.iter().map(|row| center_styled(row, "\x1b[2m", width)));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_use_tilde_and_keep_their_tail_when_narrow() {
        let home = Some(Path::new("/Users/me"));
        assert_eq!(home_relative(Path::new("/Users/me"), home), "~");
        assert_eq!(
            home_relative(Path::new("/Users/me/code/app"), home),
            "~/code/app"
        );
        assert_eq!(home_relative(Path::new("/Users/mex"), home), "/Users/mex");
        assert_eq!(home_relative(Path::new("/srv/app"), home), "/srv/app");
        assert_eq!(home_relative(Path::new("/srv/app"), None), "/srv/app");
        assert_eq!(
            home_relative(Path::new("/srv/app"), Some(Path::new("/"))),
            "/srv/app"
        );
        assert_eq!(
            home_relative(Path::new("/srv/app"), Some(Path::new(""))),
            "/srv/app"
        );

        let details = WelcomeDetails::new("~/a/very/long/project/path · main", &[]);
        assert_eq!(details.rows(12), ["…path · main"]);
    }

    #[test]
    fn narrow_locations_keep_the_branch_when_the_path_is_wide() {
        let details = WelcomeDetails::new("~/项目/应用程序/代码 · main", &[]);
        let rows = details.rows(12);
        assert_eq!(rows, ["…代码 · main"]);
        assert!(markdown::visible_width(&rows[0]) <= 12);
    }

    #[test]
    fn branches_come_from_head_including_worktrees_and_detached_heads() {
        let root = crate::test_support::unique_temp_dir("welcome-git");
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("git dir");
        std::fs::create_dir_all(repo.join("src/deep")).expect("nested dir");
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feature/x\n").expect("head");
        assert_eq!(
            git_branch(&repo.join("src/deep")).as_deref(),
            Some("feature/x")
        );

        let worktree = root.join("worktree");
        std::fs::create_dir_all(root.join("repo/.git/worktrees/wt")).expect("worktree git dir");
        std::fs::write(
            root.join("repo/.git/worktrees/wt/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .expect("detached head");
        std::fs::create_dir_all(&worktree).expect("worktree");
        std::fs::write(worktree.join(".git"), "gitdir: ../repo/.git/worktrees/wt\n")
            .expect("pointer");
        assert_eq!(git_branch(&worktree).as_deref(), Some("0123456"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
