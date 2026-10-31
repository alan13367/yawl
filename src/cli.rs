use std::time::Duration;

pub(super) const HELP: &str = "\
yawl - a minimal, self-extending AI agent harness

Usage:
  yawl                         Open the full-screen terminal UI
  yawl \"PROMPT\"                Run one turn and stream plain text
  command | yawl              Read the prompt from standard input

Options:
  -m, --model MODEL           Override the configured model
  -c, --continue              Resume the most recent session
      --session ID            Resume a session by id
      --trust-project         Trust project skills and provider settings for this invocation
      --login PROVIDER        Log into a subscription provider
      --setup                 Run provider and model onboarding again
      --doctor                Diagnose and repair the configuration
  -h, --help                  Show this help
  -V, --version               Show the version

One-shot runs (for scripts and other agents):
      --format FORMAT         Output text (default), json (one result object),
                              or jsonl (one event per line, then the result)
      --json                  Same as --format json
      --no-stdin              Never read standard input; use only the prompt arguments
      --reasoning EFFORT      Reasoning effort for this run, such as low or high
      --timeout SECONDS       Stop the run after this many seconds (exit 124)
      --agent PRESET          Run with a subagent preset's tools and role, such as scout
      --ephemeral             Do not save the session

Inspection (add --json for machine-readable output):
      --check                 Report whether yawl is ready to run a turn
      --list-models           List configured providers and models
      --refresh               With --list-models or --check, query providers over the network
      --list-sessions         List resumable sessions for this directory
      --list-agents           List subagent presets usable with --agent
      --list-tools            List builtin and custom tools

Integration:
      --print-skill           Print the skill that teaches other agent harnesses to use yawl
      --install-skill TARGET  Install that skill for claude, codex, opencode, agents, all, or a directory

Exit codes: 0 success, 1 failure, 2 usage, 3 configuration or authentication,
4 provider, 124 timeout, 130 interrupted.
";

/// How a one-shot run or an inspection command writes its output.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    #[default]
    Text,
    Json,
    Jsonl,
}

impl Format {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "jsonl" | "stream-json" => Ok(Self::Jsonl),
            _ => Err(format!(
                "unknown format '{value}'; expected text, json, or jsonl"
            )),
        }
    }

    pub(crate) fn is_machine(self) -> bool {
        self != Self::Text
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Cli {
    pub(super) model: Option<String>,
    pub(super) continue_latest: bool,
    pub(super) session_id: Option<String>,
    pub(super) list_tools: bool,
    pub(super) list_models: bool,
    pub(super) list_sessions: bool,
    pub(super) list_agents: bool,
    pub(super) check: bool,
    pub(super) refresh: bool,
    pub(super) print_skill: bool,
    pub(super) install_skill: Option<String>,
    pub(super) trust_project: bool,
    pub(super) login: Option<String>,
    pub(super) setup: bool,
    pub(super) doctor: bool,
    pub(super) help: bool,
    pub(super) version: bool,
    pub(super) format: Format,
    pub(super) no_stdin: bool,
    pub(super) reasoning: Option<String>,
    pub(super) timeout: Option<Duration>,
    pub(super) agent: Option<String>,
    pub(super) ephemeral: bool,
    pub(super) prompt: Vec<String>,
}

impl Cli {
    /// Options that only shape a one-shot run, named for error messages.
    pub(super) fn print_only_option(&self) -> Option<&'static str> {
        if self.format.is_machine() {
            Some("--format")
        } else if self.no_stdin {
            Some("--no-stdin")
        } else if self.timeout.is_some() {
            Some("--timeout")
        } else if self.agent.is_some() {
            Some("--agent")
        } else if self.ephemeral {
            Some("--ephemeral")
        } else {
            None
        }
    }
}

/// Whether the arguments ask for machine-readable output, judged without a
/// full parse so even a usage error can be reported in the requested format.
pub(super) fn wants_machine_output(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| *arg != "--")
        .zip(args.iter().skip(1).map(Some).chain([None]))
        .any(|(arg, next)| match arg.as_str() {
            "--json" => true,
            "--format" => {
                next.is_some_and(|value| Format::parse(value).is_ok_and(Format::is_machine))
            }
            _ => false,
        })
}

pub(super) fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut args = args.into_iter();
    let mut positional_only = false;
    while let Some(arg) = args.next() {
        if positional_only {
            cli.prompt.push(arg);
            continue;
        }
        let mut value = |what: &str| {
            args.next()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("{arg} requires {what}"))
        };
        match arg.as_str() {
            "--" => positional_only = true,
            "-m" | "--model" => cli.model = Some(value("a model name")?),
            "-c" | "--continue" => cli.continue_latest = true,
            "--session" => cli.session_id = Some(value("an id")?),
            "--list-tools" => cli.list_tools = true,
            "--list-models" => cli.list_models = true,
            "--list-sessions" => cli.list_sessions = true,
            "--list-agents" => cli.list_agents = true,
            "--check" => cli.check = true,
            "--refresh" => cli.refresh = true,
            "--print-skill" => cli.print_skill = true,
            "--install-skill" => cli.install_skill = Some(value("a target")?),
            "--trust-project" => cli.trust_project = true,
            "--login" => cli.login = Some(value("a provider")?),
            "--setup" => cli.setup = true,
            "--doctor" => cli.doctor = true,
            "-h" | "--help" => cli.help = true,
            "-V" | "--version" => cli.version = true,
            "--format" => cli.format = Format::parse(&value("a format")?)?,
            "--json" => cli.format = Format::Json,
            "--no-stdin" => cli.no_stdin = true,
            "--reasoning" => cli.reasoning = Some(value("an effort")?),
            "--timeout" => cli.timeout = Some(parse_timeout(&value("a number of seconds")?)?),
            "--agent" => cli.agent = Some(value("a preset name")?),
            "--ephemeral" => cli.ephemeral = true,
            _ if arg.starts_with('-') => return Err(format!("unknown option '{arg}'")),
            _ => cli.prompt.push(arg),
        }
    }
    validate(&cli)?;
    Ok(cli)
}

fn parse_timeout(value: &str) -> Result<Duration, String> {
    match value.parse::<f64>() {
        Ok(seconds) if seconds.is_finite() && seconds > 0.0 && seconds <= 31_536_000.0 => {
            Ok(Duration::from_secs_f64(seconds))
        }
        _ => Err(format!(
            "--timeout expects a positive number of seconds, not '{value}'"
        )),
    }
}

/// Rejects option combinations that cannot mean anything, naming the
/// options involved so a calling agent can correct itself.
fn validate(cli: &Cli) -> Result<(), String> {
    if cli.continue_latest && cli.session_id.is_some() {
        return Err("--continue and --session cannot be used together".into());
    }
    if cli.ephemeral && (cli.continue_latest || cli.session_id.is_some()) {
        return Err("--ephemeral cannot resume a session".into());
    }
    let actions = [
        ("--list-tools", cli.list_tools),
        ("--list-models", cli.list_models),
        ("--list-sessions", cli.list_sessions),
        ("--list-agents", cli.list_agents),
        ("--check", cli.check),
        ("--print-skill", cli.print_skill),
        ("--install-skill", cli.install_skill.is_some()),
        ("--login", cli.login.is_some()),
        ("--setup", cli.setup),
        ("--doctor", cli.doctor),
    ]
    .into_iter()
    .filter_map(|(name, set)| set.then_some(name))
    .collect::<Vec<_>>();
    if let [first, second, ..] = actions[..] {
        return Err(format!("{first} and {second} cannot be used together"));
    }
    if cli.refresh && !cli.list_models && !cli.check {
        return Err("--refresh works only with --list-models or --check".into());
    }
    let Some(action) = actions.first().copied() else {
        return Ok(());
    };
    if matches!(action, "--login" | "--setup" | "--doctor") {
        let extra = cli.model.is_some()
            || cli.format.is_machine()
            || turn_option(cli).is_some()
            || (action != "--setup" && cli.trust_project);
        if extra {
            return Err("--login, --setup, and --doctor must be used alone".into());
        }
        return Ok(());
    }
    if let Some(option) = turn_option(cli) {
        return Err(format!("{option} cannot be used with {action}"));
    }
    if cli.model.is_some() && matches!(action, "--print-skill" | "--install-skill") {
        return Err(format!("--model cannot be used with {action}"));
    }
    if cli.format.is_machine() && action == "--print-skill" {
        return Err(
            "--print-skill prints Markdown; it cannot be combined with --json or --format".into(),
        );
    }
    Ok(())
}

/// The first option that only makes sense for a turn.
fn turn_option(cli: &Cli) -> Option<&'static str> {
    if !cli.prompt.is_empty() {
        Some("a prompt")
    } else if cli.continue_latest {
        Some("--continue")
    } else if cli.session_id.is_some() {
        Some("--session")
    } else if cli.reasoning.is_some() {
        Some("--reasoning")
    } else if cli.no_stdin {
        Some("--no-stdin")
    } else if cli.timeout.is_some() {
        Some("--timeout")
    } else if cli.agent.is_some() {
        Some("--agent")
    } else if cli.ephemeral {
        Some("--ephemeral")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(items: &[&str]) -> Result<Cli, String> {
        parse_args(items.iter().map(|item| (*item).to_string()))
    }

    #[test]
    fn parses_model_session_and_prompt() {
        let cli = parse(&[
            "-m",
            "openai:gpt-4o",
            "--session",
            "abc-1",
            "hello",
            "world",
        ])
        .expect("arguments should parse");
        assert_eq!(cli.model.as_deref(), Some("openai:gpt-4o"));
        assert_eq!(cli.session_id.as_deref(), Some("abc-1"));
        assert_eq!(cli.prompt, ["hello", "world"]);
    }

    #[test]
    fn rejects_conflicting_session_flags() {
        assert!(parse(&["-c", "--session", "abc"]).is_err());
        assert!(parse(&["--ephemeral", "-c", "hi"]).is_err());
        assert!(parse(&["--ephemeral", "--session", "abc", "hi"]).is_err());
    }

    #[test]
    fn parses_project_trust_override_with_a_prompt() {
        let cli = parse(&["--trust-project", "inspect this"])
            .expect("the trust override should compose with a turn");
        assert!(cli.trust_project);
        assert_eq!(cli.prompt, ["inspect this"]);
    }

    #[test]
    fn parses_standalone_login() {
        let cli = parse(&["--login", "openai-codex"]).expect("login should parse");
        assert_eq!(cli.login.as_deref(), Some("openai-codex"));
        assert!(parse(&["--login", "openai-codex", "prompt"]).is_err());
        assert!(parse(&["--login", "openai-codex", "--trust-project"]).is_err());
        assert!(parse(&["--setup", "--login", "openai-codex"]).is_err());
    }

    #[test]
    fn setup_accepts_project_trust_override_but_no_other_turn_options() {
        let cli = parse(&["--setup", "--trust-project"]).expect("setup can trust the project");
        assert!(cli.setup);
        assert!(cli.trust_project);
        assert!(parse(&["--trust-project", "--setup"]).is_ok());
        assert!(parse(&["--setup", "prompt"]).is_err());
        assert!(parse(&["--setup", "-m", "openai:gpt-4o"]).is_err());
    }

    #[test]
    fn parses_standalone_doctor() {
        let cli = parse(&["--doctor"]).expect("doctor should parse");
        assert!(cli.doctor);
        assert!(parse(&["--doctor", "--setup"]).is_err());
        assert!(parse(&["--doctor", "-m", "openai:gpt-4o"]).is_err());
        assert!(parse(&["--doctor", "prompt"]).is_err());
        assert!(parse(&["--doctor", "--json"]).is_err());
    }

    #[test]
    fn double_dash_allows_dash_prefixed_prompt() {
        let cli = parse(&["--", "--explain"]).expect("arguments should parse");
        assert_eq!(cli.prompt, ["--explain"]);
    }

    #[test]
    fn parses_one_shot_run_options() {
        let cli = parse(&[
            "--format",
            "jsonl",
            "--no-stdin",
            "--reasoning",
            "high",
            "--timeout",
            "1.5",
            "--agent",
            "scout",
            "--ephemeral",
            "find the bug",
        ])
        .expect("run options should parse");
        assert_eq!(cli.format, Format::Jsonl);
        assert!(cli.no_stdin);
        assert_eq!(cli.reasoning.as_deref(), Some("high"));
        assert_eq!(cli.timeout, Some(Duration::from_millis(1500)));
        assert_eq!(cli.agent.as_deref(), Some("scout"));
        assert!(cli.ephemeral);
        assert_eq!(parse(&["--json", "x"]).unwrap().format, Format::Json);
        assert_eq!(
            parse(&["--format", "stream-json", "x"]).unwrap().format,
            Format::Jsonl
        );
    }

    #[test]
    fn rejects_bad_option_values() {
        for args in [
            &["--format", "yaml", "x"][..],
            &["--timeout", "0", "x"],
            &["--timeout", "-3", "x"],
            &["--timeout", "soon", "x"],
            &["--timeout", "NaN", "x"],
            &["--reasoning"],
            &["--model", ""],
        ] {
            assert!(parse(args).is_err(), "{args:?} should be rejected");
        }
    }

    #[test]
    fn inspection_commands_are_exclusive_and_take_no_turn_options() {
        assert!(parse(&["--list-models", "--json", "--refresh"]).is_ok());
        assert!(parse(&["--check", "--json", "-m", "openai:gpt-4o"]).is_ok());
        assert!(parse(&["--list-sessions", "--format", "json"]).is_ok());
        assert!(parse(&["--list-agents"]).is_ok());
        assert!(parse(&["--list-models", "--list-tools"]).is_err());
        assert!(parse(&["--check", "prompt"]).is_err());
        assert!(parse(&["--list-models", "--timeout", "5"]).is_err());
        assert!(parse(&["--list-sessions", "--refresh"]).is_err());
        assert!(parse(&["--refresh", "prompt"]).is_err());
        assert!(parse(&["--print-skill", "-m", "x"]).is_err());
        assert!(parse(&["--print-skill", "--json"]).is_err());
        assert!(parse(&["--install-skill", "all", "--json"]).is_ok());
        let cli = parse(&["--install-skill", "claude"]).expect("install should parse");
        assert_eq!(cli.install_skill.as_deref(), Some("claude"));
        assert!(parse(&["--install-skill"]).is_err());
    }

    #[test]
    fn machine_output_is_detected_without_a_full_parse() {
        let args = |items: &[&str]| {
            items
                .iter()
                .map(|item| (*item).to_string())
                .collect::<Vec<_>>()
        };
        assert!(wants_machine_output(&args(&["--json", "--bogus"])));
        assert!(wants_machine_output(&args(&["--format", "jsonl"])));
        assert!(!wants_machine_output(&args(&["--format", "text"])));
        assert!(!wants_machine_output(&args(&["--format"])));
        assert!(!wants_machine_output(&args(&["--", "--json"])));
    }
}
