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
      --list-tools            List builtin and discovered exec tools
      --trust-project         Allow project skill sources for this invocation
      --login PROVIDER        Log into a subscription provider
      --setup                 Run provider and model onboarding again
      --doctor                Diagnose and repair the configuration
  -h, --help                  Show this help
  -V, --version               Show the version
";

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Cli {
    pub(super) model: Option<String>,
    pub(super) continue_latest: bool,
    pub(super) session_id: Option<String>,
    pub(super) list_tools: bool,
    pub(super) trust_project: bool,
    pub(super) login: Option<String>,
    pub(super) setup: bool,
    pub(super) doctor: bool,
    pub(super) help: bool,
    pub(super) version: bool,
    pub(super) prompt: Vec<String>,
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
        match arg.as_str() {
            "--" => positional_only = true,
            "-m" | "--model" => {
                let model = args
                    .next()
                    .ok_or_else(|| format!("{arg} requires a model name"))?;
                if model.is_empty() {
                    return Err("model name must not be empty".into());
                }
                cli.model = Some(model);
            }
            "-c" | "--continue" => cli.continue_latest = true,
            "--session" => {
                cli.session_id = Some(
                    args.next()
                        .ok_or_else(|| "--session requires an id".to_string())?,
                );
            }
            "--list-tools" => cli.list_tools = true,
            "--trust-project" => cli.trust_project = true,
            "--login" => {
                cli.login = Some(
                    args.next()
                        .ok_or_else(|| "--login requires a provider".to_string())?,
                );
            }
            "--setup" => cli.setup = true,
            "--doctor" => cli.doctor = true,
            "-h" | "--help" => cli.help = true,
            "-V" | "--version" => cli.version = true,
            _ if arg.starts_with('-') => return Err(format!("unknown option '{arg}'")),
            _ => cli.prompt.push(arg),
        }
    }
    if cli.continue_latest && cli.session_id.is_some() {
        return Err("--continue and --session cannot be used together".into());
    }
    if (cli.login.is_some() || cli.setup || cli.doctor)
        && (cli.model.is_some()
            || cli.continue_latest
            || cli.session_id.is_some()
            || cli.list_tools
            || cli.trust_project
            || !cli.prompt.is_empty()
            || cli.login.is_some() && (cli.setup || cli.doctor)
            || cli.setup && cli.doctor)
    {
        return Err("--login, --setup, and --doctor must be used alone".into());
    }
    Ok(cli)
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
        assert!(parse(&["--setup"]).is_ok());
        assert!(parse(&["--setup", "--login", "openai-codex"]).is_err());
    }

    #[test]
    fn parses_standalone_doctor() {
        let cli = parse(&["--doctor"]).expect("doctor should parse");
        assert!(cli.doctor);
        assert!(parse(&["--doctor", "--setup"]).is_err());
        assert!(parse(&["--doctor", "-m", "openai:gpt-4o"]).is_err());
        assert!(parse(&["--doctor", "prompt"]).is_err());
    }

    #[test]
    fn double_dash_allows_dash_prefixed_prompt() {
        let cli = parse(&["--", "--explain"]).expect("arguments should parse");
        assert_eq!(cli.prompt, ["--explain"]);
    }
}
