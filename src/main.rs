mod cli;
mod print_mode;

use std::io::{self, IsTerminal};

use cli::{Cli, HELP, parse_args};
use print_mode::{read_prompt, run as run_print_mode};
use yawl::agent::Agent;
use yawl::config::Config;
use yawl::error::Error;
use yawl::session::Session;
use yawl::tools::{DescribeCache, Registry};

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
        println!("{}", resume_command(agent.session_id()));
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

fn resume_command(session_id: &str) -> String {
    format!("yawl --session {session_id}")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_command_is_copy_pasteable() {
        assert_eq!(
            resume_command("20260820-093301-1a2b"),
            "yawl --session 20260820-093301-1a2b"
        );
    }
}
