use std::io::{self, IsTerminal, Read, Write};

use yawl::agent::{Agent, TurnEvent};
use yawl::config::Config;
use yawl::error::Error;
use yawl::provider::{Reasoning, ReasoningKind};
use yawl::session::Session;
use yawl::tools::{DescribeCache, Registry};

const HELP: &str = "\
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
      --login PROVIDER        Log into a subscription provider
      --setup                 Run provider and model onboarding again
  -h, --help                  Show this help
  -V, --version               Show the version
";

#[derive(Debug, Default, PartialEq, Eq)]
struct Cli {
    model: Option<String>,
    continue_latest: bool,
    session_id: Option<String>,
    list_tools: bool,
    login: Option<String>,
    setup: bool,
    help: bool,
    version: bool,
    prompt: Vec<String>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Cli, String> {
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
            "--login" => {
                cli.login = Some(
                    args.next()
                        .ok_or_else(|| "--login requires a provider".to_string())?,
                );
            }
            "--setup" => cli.setup = true,
            "-h" | "--help" => cli.help = true,
            "-V" | "--version" => cli.version = true,
            _ if arg.starts_with('-') => return Err(format!("unknown option '{arg}'")),
            _ => cli.prompt.push(arg),
        }
    }
    if cli.continue_latest && cli.session_id.is_some() {
        return Err("--continue and --session cannot be used together".into());
    }
    if (cli.login.is_some() || cli.setup)
        && (cli.model.is_some()
            || cli.continue_latest
            || cli.session_id.is_some()
            || cli.list_tools
            || !cli.prompt.is_empty()
            || cli.login.is_some() && cli.setup)
    {
        return Err("--login and --setup must be used alone".into());
    }
    Ok(cli)
}

fn main() {
    match run() {
        Ok(0) => {}
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("yawl: {error}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<i32, Box<dyn std::error::Error>> {
    let cli = parse_args(std::env::args().skip(1)).map_err(|message| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{message}\nTry 'yawl --help' for usage."),
        )
    })?;
    if cli.help {
        print!("{HELP}");
        return Ok(0);
    }
    if cli.version {
        println!("yawl {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }

    let mut config = Config::load()?;
    if let Some(provider) = &cli.login {
        yawl::install_interrupt_handler()?;
        match provider.as_str() {
            "openai-codex" | "codex" => yawl::provider::codex::login(&config)?,
            _ => {
                return Err(Box::new(Error::Config(format!(
                    "unsupported login provider '{provider}'; supported: openai-codex"
                ))));
            }
        }
        return Ok(0);
    }
    if cli.setup {
        yawl::install_interrupt_handler()?;
        let _ = yawl::onboarding::run(&config)?;
        return Ok(0);
    }
    if let Some(model) = &cli.model {
        config.model = Some(model.clone());
    }
    if cli.list_tools {
        list_tools(&config);
        return Ok(0);
    }

    let stdin_is_terminal = io::stdin().is_terminal();
    if config.model.is_none() && cli.prompt.is_empty() && stdin_is_terminal {
        yawl::install_interrupt_handler()?;
        config = yawl::onboarding::run(&config)?;
    }
    let model = config.model.clone().ok_or_else(|| {
        Error::Config(
            "no model configured; run 'yawl' in a terminal for setup or pass --model".into(),
        )
    })?;
    let (session, messages) = open_session(&config, &cli)?;
    let mut agent = Agent::new(config, model, session, messages);

    if cli.prompt.is_empty() && stdin_is_terminal {
        yawl::tui::run(&mut agent)?;
        return Ok(0);
    }

    let prompt = read_prompt(cli.prompt, stdin_is_terminal)?;
    yawl::install_interrupt_handler()?;
    run_print_mode(&mut agent, prompt)
}

fn open_session(
    config: &Config,
    cli: &Cli,
) -> Result<(Session, Vec<yawl::provider::Message>), Error> {
    if let Some(id) = &cli.session_id {
        return Session::open(&config.sessions_dir(), id);
    }
    if cli.continue_latest
        && let Some(session) = Session::open_latest(&config.sessions_dir())?
    {
        return Ok(session);
    }
    Ok((Session::create(&config.sessions_dir())?, Vec::new()))
}

fn read_prompt(words: Vec<String>, stdin_is_terminal: bool) -> Result<String, io::Error> {
    let positional = words.join(" ");
    let mut piped = String::new();
    if !stdin_is_terminal {
        io::stdin().read_to_string(&mut piped)?;
    }
    let prompt = match (positional.is_empty(), piped.is_empty()) {
        (false, false) => format!("{positional}\n{piped}"),
        (false, true) => positional,
        (true, false) => piped,
        (true, true) => String::new(),
    };
    if prompt.trim().is_empty() {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "prompt is empty",
        ))
    } else {
        Ok(prompt)
    }
}

fn list_tools(config: &Config) {
    let mut cache = DescribeCache::default();
    let registry = Registry::scan(config, &mut cache);
    for (name, description, origin) in registry.describe_all() {
        println!("{name}\t{origin}\n  {description}");
    }
    for warning in registry.warnings {
        eprintln!("warning: {warning}");
    }
}

fn run_print_mode(agent: &mut Agent, prompt: String) -> Result<i32, Box<dyn std::error::Error>> {
    let mut stdout = io::stdout().lock();
    let hide_reasoning = agent.config.hide_reasoning;
    let mut pending_reasoning = Vec::new();
    let mut response_has_text = false;
    let mut output_error = None;
    let completed = agent.run_turn(Some(prompt), &mut |event| match event {
        TurnEvent::TextDelta(text) => {
            print_reasoning(&mut pending_reasoning);
            if output_error.is_none() {
                match stdout
                    .write_all(text.as_bytes())
                    .and_then(|()| stdout.flush())
                {
                    Ok(()) => response_has_text = true,
                    Err(error) => {
                        output_error = Some(error);
                        yawl::set_interrupted(true);
                    }
                }
            }
        }
        TurnEvent::ReasoningDelta { kind, text } => {
            if !hide_reasoning {
                append_reasoning(&mut pending_reasoning, kind, text);
            }
        }
        TurnEvent::RetryReset => {
            eprintln!("\nretry restarted the response; earlier partial text may repeat");
            pending_reasoning.clear();
            response_has_text = false;
        }
        TurnEvent::Retrying {
            attempt,
            delay_ms,
            error,
        } => eprintln!("\nrequest attempt {attempt} failed ({error}); retrying in {delay_ms}ms"),
        TurnEvent::AssistantDone => {
            print_reasoning(&mut pending_reasoning);
            if response_has_text && output_error.is_none() {
                if let Err(error) = stdout.write_all(b"\n").and_then(|()| stdout.flush()) {
                    output_error = Some(error);
                    yawl::set_interrupted(true);
                }
                response_has_text = false;
            }
        }
        TurnEvent::Compacting => eprintln!("compacting conversation..."),
        TurnEvent::Compacted { replaced } => {
            eprintln!("compacted {replaced} messages")
        }
        TurnEvent::ToolStart { .. } | TurnEvent::ToolEnd { .. } | TurnEvent::Usage { .. } => {}
    });
    if let Some(error) = output_error {
        return Err(Box::new(error));
    }
    let completed = completed?;
    if completed {
        Ok(0)
    } else {
        eprintln!("turn interrupted");
        Ok(130)
    }
}

fn append_reasoning(reasoning: &mut Vec<Reasoning>, kind: ReasoningKind, text: &str) {
    if let Some(current) = reasoning.last_mut()
        && current.kind == kind
    {
        current.content.push_str(text);
    } else {
        reasoning.push(Reasoning {
            kind,
            content: text.to_string(),
        });
    }
}

fn print_reasoning(reasoning: &mut Vec<Reasoning>) {
    for block in reasoning.drain(..) {
        match block.kind {
            ReasoningKind::Summary => {
                let summary = block
                    .content
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                eprintln!("{summary}");
            }
            ReasoningKind::Full => eprintln!("{}", block.content.trim()),
        }
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
    fn double_dash_allows_dash_prefixed_prompt() {
        let cli = parse(&["--", "--explain"]).expect("arguments should parse");
        assert_eq!(cli.prompt, ["--explain"]);
    }
}
