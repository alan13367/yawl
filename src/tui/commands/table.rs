//! Every slash command, defined once: the names it answers to, its help and
//! completion text, and whether it also runs while a turn is busy. Idle and
//! busy dispatch, `/help`, and the completion menu all read this table.

/// A slash command the TUI handles itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tui) enum Command {
    Model,
    Reasoning,
    Connect,
    Settings,
    Subagents,
    Git,
    Processes,
    Remote,
    New,
    Compact,
    Usage,
    Undo,
    Continue,
    Init,
    Copy,
    CopyAll,
    Tools,
    Skills,
    Resume,
    Unqueue,
    Goal,
    Plan,
    Help,
    Hotkeys,
    Diff,
    Quit,
}

/// When a command also runs while a turn is busy. Anything else waits in
/// the queue until the turn settles.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Busy {
    Never,
    /// Only the bare form, which opens a picker or dashboard.
    WithoutArgument,
    Always,
}

struct Spec {
    command: Command,
    /// Completion lists every name; the first is canonical.
    names: &'static [&'static str],
    /// Accepted but never suggested.
    hidden: &'static [&'static str],
    /// Arguments shown in `/help`, such as ` [ID]`.
    arguments: &'static str,
    /// The completion menu line.
    summary: &'static str,
    /// The `/help` table line.
    help: &'static str,
    busy: Busy,
}

/// In completion-menu order: pickers and dashboards come first.
const COMMANDS: &[Spec] = &[
    Spec {
        command: Command::Model,
        names: &["model"],
        hidden: &[],
        arguments: " [MODEL]",
        summary: "List or switch models",
        help: "Open model picker or switch directly",
        busy: Busy::WithoutArgument,
    },
    Spec {
        command: Command::Reasoning,
        names: &["reasoning"],
        hidden: &[],
        arguments: " [LVL]",
        summary: "Show or set the reasoning effort",
        help: "Show supported reasoning levels or set one",
        busy: Busy::Always,
    },
    Spec {
        command: Command::Connect,
        names: &["connect"],
        hidden: &[],
        arguments: "",
        summary: "Configure a model provider",
        help: "Configure a model provider interactively",
        busy: Busy::WithoutArgument,
    },
    Spec {
        command: Command::Settings,
        names: &["settings"],
        hidden: &[],
        arguments: " [KEY]",
        summary: "Show or change settings",
        help: "Open settings picker or change directly",
        busy: Busy::WithoutArgument,
    },
    Spec {
        command: Command::Subagents,
        names: &["subagents"],
        hidden: &[],
        arguments: "",
        summary: "Open the subagent dashboard",
        help: "Open the subagent dashboard",
        busy: Busy::WithoutArgument,
    },
    Spec {
        command: Command::Git,
        names: &["git"],
        hidden: &[],
        arguments: "",
        summary: "Open the git dashboard",
        help: "Open the git dashboard (offers repo setup outside a repo)",
        busy: Busy::WithoutArgument,
    },
    Spec {
        command: Command::Processes,
        names: &["ps"],
        hidden: &[],
        arguments: "",
        summary: "Open the background process dashboard",
        help: "Open the background process dashboard",
        busy: Busy::WithoutArgument,
    },
    Spec {
        command: Command::Remote,
        names: &["remote"],
        hidden: &[],
        arguments: " [off]",
        summary: "Control this session from another device",
        help: "Control this session from a device in your tailnet",
        // Either side must be able to stop remote control mid-turn.
        busy: Busy::Always,
    },
    Spec {
        command: Command::New,
        names: &["new", "clear"],
        hidden: &[],
        arguments: "",
        summary: "Start a session without changing directories",
        help: "Start a fresh session in the current directory",
        busy: Busy::Never,
    },
    Spec {
        command: Command::Compact,
        names: &["compact"],
        hidden: &[],
        arguments: "",
        summary: "Summarize older messages",
        help: "Summarize older messages now",
        busy: Busy::Never,
    },
    Spec {
        command: Command::Usage,
        names: &["usage"],
        hidden: &[],
        arguments: "",
        summary: "Show token and prompt-cache usage",
        help: "Show token and prompt-cache usage",
        busy: Busy::WithoutArgument,
    },
    Spec {
        command: Command::Undo,
        names: &["undo"],
        hidden: &[],
        arguments: "",
        summary: "Undo the last turn",
        help: "Restore files and drop the last turn",
        busy: Busy::Never,
    },
    Spec {
        command: Command::Continue,
        names: &["continue"],
        hidden: &[],
        arguments: "",
        summary: "Resume a failed or interrupted turn",
        help: "Resume a turn that failed or was interrupted",
        busy: Busy::Never,
    },
    Spec {
        command: Command::Init,
        names: &["init"],
        hidden: &[],
        arguments: "",
        summary: "Create or update AGENTS.md",
        help: "Create or update project agent guidance",
        busy: Busy::Never,
    },
    Spec {
        command: Command::Copy,
        names: &["copy"],
        hidden: &[],
        arguments: "",
        summary: "Copy the last assistant reply",
        help: "Copy the last assistant reply",
        busy: Busy::WithoutArgument,
    },
    Spec {
        command: Command::CopyAll,
        names: &["copy-all"],
        hidden: &[],
        arguments: "",
        summary: "Copy the conversation without reasoning",
        help: "Copy conversation without reasoning",
        busy: Busy::WithoutArgument,
    },
    Spec {
        command: Command::Tools,
        names: &["tools"],
        hidden: &[],
        arguments: "",
        summary: "List available tools",
        help: "List builtin tools and your custom tools",
        busy: Busy::Never,
    },
    Spec {
        command: Command::Skills,
        names: &["skills"],
        hidden: &[],
        arguments: "",
        summary: "List available skills",
        help: "List discovered skills (run `/skill:NAME`)",
        busy: Busy::Never,
    },
    Spec {
        command: Command::Resume,
        names: &["resume"],
        hidden: &[],
        arguments: " [ID]",
        summary: "List or resume sessions",
        help: "Open session picker or resume directly",
        busy: Busy::Never,
    },
    Spec {
        command: Command::Unqueue,
        names: &["unqueue"],
        hidden: &[],
        arguments: " [N]",
        summary: "Cancel queued messages",
        help: "Cancel or manage queued messages",
        busy: Busy::Always,
    },
    Spec {
        command: Command::Goal,
        names: &["goal"],
        hidden: &[],
        arguments: " [TEXT]",
        summary: "Start, resume, or cancel a persistent goal",
        help: "Start, resume, cancel, or show goal",
        busy: Busy::Always,
    },
    Spec {
        command: Command::Plan,
        names: &["plan"],
        hidden: &[],
        arguments: " [TEXT]",
        summary: "Start, revise, implement, resume, or cancel a plan",
        help: "Start, revise, implement, resume, cancel, or show plan",
        busy: Busy::Always,
    },
    Spec {
        command: Command::Help,
        names: &["help"],
        hidden: &[],
        arguments: "",
        summary: "Show help",
        help: "Show this command reference",
        busy: Busy::Never,
    },
    Spec {
        command: Command::Hotkeys,
        names: &["hotkeys"],
        hidden: &[],
        arguments: "",
        summary: "List every keyboard shortcut",
        help: "Show keyboard shortcuts grid",
        busy: Busy::Never,
    },
    Spec {
        command: Command::Diff,
        names: &["diff"],
        hidden: &[],
        arguments: "",
        summary: "Show files changed this session",
        help: "Show files changed this session",
        busy: Busy::Never,
    },
    Spec {
        command: Command::Quit,
        names: &["quit"],
        hidden: &["q"],
        arguments: "",
        summary: "Exit Yawl",
        help: "Leave Yawl",
        busy: Busy::Never,
    },
];

/// `/help` rows, grouped by task. Every command appears exactly once.
const HELP_GROUPS: &[&[Command]] = &[
    &[
        Command::New,
        Command::Resume,
        Command::Undo,
        Command::Continue,
        Command::Diff,
        Command::Init,
        Command::Quit,
    ],
    &[
        Command::Model,
        Command::Reasoning,
        Command::Settings,
        Command::Connect,
    ],
    &[
        Command::Usage,
        Command::Compact,
        Command::Copy,
        Command::CopyAll,
        Command::Tools,
        Command::Skills,
    ],
    &[
        Command::Goal,
        Command::Plan,
        Command::Unqueue,
        Command::Subagents,
        Command::Git,
        Command::Processes,
        Command::Remote,
    ],
    &[Command::Hotkeys, Command::Help],
];

const HELP_INPUT: &str = "
Input

- `Enter` submits when idle, or steers while a response runs
- `Enter` on an empty prompt continues a turn that just failed
- `Tab` completes in menus, or queues a message while busy
- Run `/hotkeys` for all editing, transcript, and dashboard shortcuts
";

fn spec(command: Command) -> &'static Spec {
    COMMANDS
        .iter()
        .find(|spec| spec.command == command)
        .unwrap_or(&COMMANDS[0])
}

/// A typed slash command: the name as typed, its trimmed argument, and the
/// command it names, if any.
pub(in crate::tui) struct Typed<'a> {
    pub(in crate::tui) name: &'a str,
    pub(in crate::tui) argument: &'a str,
    pub(in crate::tui) command: Option<Command>,
}

/// Splits `/name argument`. `None` when `text` is not a slash command.
pub(in crate::tui) fn parse(text: &str) -> Option<Typed<'_>> {
    let rest = text.trim().strip_prefix('/')?;
    let (name, argument) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(name, argument)| (name, argument.trim()));
    let command = COMMANDS
        .iter()
        .find(|spec| spec.names.contains(&name) || spec.hidden.contains(&name))
        .map(|spec| spec.command);
    Some(Typed {
        name,
        argument,
        command,
    })
}

/// The command and argument in `text` when that command also runs while a
/// turn is busy.
pub(in crate::tui) fn busy_command(text: &str) -> Option<(Command, String)> {
    let typed = parse(text)?;
    let command = typed.command?;
    let runs = match spec(command).busy {
        Busy::Never => false,
        Busy::WithoutArgument => typed.argument.is_empty(),
        Busy::Always => true,
    };
    runs.then(|| (command, typed.argument.to_string()))
}

/// Completion menu entries as `(command, description)`, aliases after their
/// command.
pub(in crate::tui) fn menu() -> Vec<(String, String)> {
    COMMANDS
        .iter()
        .flat_map(|spec| {
            spec.names.iter().enumerate().map(|(index, name)| {
                let description = if index == 0 {
                    spec.summary.to_string()
                } else {
                    format!("Alias for /{}", spec.names[0])
                };
                (format!("/{name}"), description)
            })
        })
        .collect()
}

/// The `/help` command reference.
pub(in crate::tui) fn help() -> String {
    let mut text = String::from("Commands\n\n| Command | Description |\n| --- | --- |\n");
    for (index, group) in HELP_GROUPS.iter().enumerate() {
        if index > 0 {
            text.push_str("| --- | --- |\n");
        }
        for command in *group {
            let spec = spec(*command);
            let usage = spec
                .names
                .iter()
                .map(|name| format!("`/{name}{}`", spec.arguments))
                .collect::<Vec<_>>()
                .join(", ");
            text.push_str(&format!("| {usage} | {} |\n", spec.help));
        }
    }
    text.push_str(HELP_INPUT);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_has_one_spec_and_one_help_row() {
        for spec in COMMANDS {
            assert_eq!(
                COMMANDS
                    .iter()
                    .filter(|other| other.command == spec.command)
                    .count(),
                1,
                "{:?}",
                spec.command
            );
            assert_eq!(
                HELP_GROUPS
                    .iter()
                    .flat_map(|group| group.iter())
                    .filter(|command| **command == spec.command)
                    .count(),
                1,
                "{:?} must appear once in /help",
                spec.command
            );
            for name in spec.names.iter().chain(spec.hidden) {
                assert_eq!(
                    parse(&format!("/{name}")).unwrap().command,
                    Some(spec.command)
                );
            }
        }
    }

    #[test]
    fn parsing_keeps_unknown_names_and_trims_arguments() {
        let typed = parse("  /resume   2026-1  ").unwrap();
        assert_eq!(typed.command, Some(Command::Resume));
        assert_eq!(typed.argument, "2026-1");
        let unknown = parse("/frobnicate now").unwrap();
        assert_eq!(unknown.name, "frobnicate");
        assert_eq!(unknown.command, None);
        assert!(parse("hello").is_none());
        assert_eq!(parse("/q").unwrap().command, Some(Command::Quit));
    }

    #[test]
    fn help_lists_aliases_and_arguments_and_menu_hides_hidden_aliases() {
        let help = help();
        assert!(
            help.contains("| `/new`, `/clear` | Start a fresh session in the current directory |")
        );
        assert!(help.contains("| `/resume [ID]` | Open session picker or resume directly |"));
        let menu = menu();
        assert_eq!(menu[0].0, "/model");
        assert!(menu.contains(&("/clear".into(), "Alias for /new".into())));
        assert!(!menu.iter().any(|(command, _)| command == "/q"));
    }
}
