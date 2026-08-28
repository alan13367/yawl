mod cli;
mod print_mode;
mod project_trust;

use std::io::{self, IsTerminal};
use std::path::Path;

use cli::{Cli, HELP, parse_args};
use print_mode::{read_prompt, run as run_print_mode};
use yawl::agent::Agent;
use yawl::config::{Config, SessionDirs};
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
    if cli.doctor {
        yawl::install_interrupt_handler()?;
        return Ok(yawl::doctor::run()?);
    }

    let mut config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("yawl: {error}");
            eprintln!("Run 'yawl --doctor' to diagnose and repair the configuration.");
            return Ok(1);
        }
    };
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
        yawl::onboarding::run(&config)?;
        return Ok(0);
    }
    if let Some(model) = &cli.model {
        config.model = Some(model.clone());
    }
    let stdin_is_terminal = io::stdin().is_terminal();
    if cli.list_tools {
        project_trust::resolve(&mut config, cli.trust_project, stdin_is_terminal)?;
        list_tools(&config);
        return Ok(0);
    }

    if config.model.is_none() && !config.setup_skipped && cli.prompt.is_empty() && stdin_is_terminal
    {
        yawl::install_interrupt_handler()?;
        config = match yawl::onboarding::run(&config)? {
            yawl::onboarding::SetupOutcome::Configured(config) => *config,
            yawl::onboarding::SetupOutcome::Skipped => return Ok(0),
        };
    }
    let model = config.model.clone().ok_or_else(|| {
        Error::Config("no model configured; run 'yawl --setup' or pass --model".into())
    })?;
    project_trust::resolve(&mut config, cli.trust_project, stdin_is_terminal)?;
    let (session, messages) = open_session(&config, &cli, &model)?;
    let mut agent = Agent::new(config, model, session, messages);

    if cli.prompt.is_empty() && stdin_is_terminal {
        yawl::tui::run(&mut agent)?;
        if agent.discard_if_empty()? {
            return Ok(0);
        }
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
    model: &str,
) -> Result<(Session, Vec<yawl::provider::Message>), Error> {
    let cwd = yawl::config::working_dir();
    let dirs = config.session_dirs(&cwd);
    select_session(cli, &cwd, &dirs, model)
}

fn select_session(
    cli: &Cli,
    cwd: &Path,
    dirs: &SessionDirs,
    model: &str,
) -> Result<(Session, Vec<yawl::provider::Message>), Error> {
    if let Some(id) = &cli.session_id {
        return Session::open_searching(&dirs.search, id);
    }
    if cli.continue_latest
        && let Some(session) = Session::open_latest(&dirs.project)?
    {
        return Ok(session);
    }
    Ok((Session::create(&dirs.project, cwd, model)?, Vec::new()))
}

fn resume_command(session_id: &str) -> String {
    format!("yawl --session {session_id}")
}

fn list_tools(config: &Config) {
    let mut cache = DescribeCache::default();
    let registry = Registry::scan_for_main_listing(config, &mut cache);
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

    fn temp_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "yawl-main-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    /// Writes a session log with a single user message so `-c` / list treat it
    /// as resumable.
    fn write_session(dir: &Path, id: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let meta = format!(
            r#"{{"type":"meta","id":"{id}","created_unix":1,"cwd":"/proj","model":"test-model"}}"#
        );
        let message = r#"{"type":"message","message":{"role":"user","content":"hello"}}"#;
        std::fs::write(
            dir.join(format!("{id}.jsonl")),
            format!("{meta}\n{message}\n"),
        )
        .unwrap();
    }

    fn parse(parts: &[&str]) -> Cli {
        parse_args(parts.iter().map(|part| part.to_string())).expect("args should parse")
    }

    #[test]
    fn continue_latest_uses_only_the_current_project() {
        let root = temp_root("continue-current-project");
        let project = root.join("proj");
        let other = root.join("other");
        write_session(&other, "20260101-000000-0001");
        write_session(&project, "20260102-000000-0001");
        let dirs = SessionDirs {
            project: project.clone(),
            search: vec![project.clone(), other],
        };

        let (session, messages) =
            select_session(&parse(&["-c"]), Path::new("/proj"), &dirs, "test-model").unwrap();
        assert_eq!(session.id, "20260102-000000-0001");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "hello");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn session_flag_searches_current_and_other_projects() {
        let root = temp_root("session-flag-search");
        let project = root.join("proj");
        let other = root.join("other");
        write_session(&other, "20260101-000000-0002");
        let dirs = SessionDirs {
            project: project.clone(),
            search: vec![project, other],
        };

        let (session, _) = select_session(
            &parse(&["--session", "20260101-000000-0002"]),
            Path::new("/proj"),
            &dirs,
            "test-model",
        )
        .unwrap();
        assert_eq!(session.id, "20260101-000000-0002");

        let error = select_session(
            &parse(&["--session", "20990101-000000-0000"]),
            Path::new("/proj"),
            &dirs,
            "test-model",
        )
        .unwrap_err();
        assert!(error.to_string().contains("not found"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plain_start_creates_a_scoped_session_with_model() {
        let root = temp_root("plain-start");
        let project = root.join("proj");
        let dirs = SessionDirs {
            project: project.clone(),
            search: vec![project.clone()],
        };

        let (session, messages) =
            select_session(&parse(&[]), Path::new("/proj"), &dirs, "test-model").unwrap();
        assert!(messages.is_empty());
        assert!(project.join(format!("{}.jsonl", session.id)).exists());

        // Meta-only sessions are not listed until a turn is written.
        let infos = yawl::session::list(&project).unwrap();
        assert!(infos.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn continue_skips_meta_only_sessions() {
        let root = temp_root("continue-skip-empty");
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("20260101-000000-0001.jsonl"),
            r#"{"type":"meta","id":"20260101-000000-0001","created_unix":1,"cwd":"/proj","model":"test-model"}"#,
        )
        .unwrap();
        let dirs = SessionDirs {
            project: project.clone(),
            search: vec![project.clone()],
        };

        let (session, messages) =
            select_session(&parse(&["-c"]), Path::new("/proj"), &dirs, "test-model").unwrap();
        assert!(messages.is_empty());
        assert_ne!(session.id, "20260101-000000-0001");
        let _ = std::fs::remove_dir_all(&root);
    }
}
