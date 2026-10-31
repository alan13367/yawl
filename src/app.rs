//! Command-line startup: argument handling, session selection, and TUI or
//! print-mode dispatch.

use std::io::{self, IsTerminal};
use std::path::Path;

use crate::agent::Agent;
use crate::cli::{Cli, HELP, parse_args};
use crate::config::{Config, SessionDirs};
use crate::error::Error;
use crate::exit_status::Failure;
use crate::print_mode::{RunOptions, read_prompt, run as run_print_mode};
use crate::session::Session;

/// Runs the `yawl` command line and exits with its status.
pub fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let machine = crate::cli::wants_machine_output(&args);
    match run(args) {
        Ok(0) => {}
        Ok(code) => std::process::exit(code),
        Err(error) => {
            let failure = crate::exit_status::classify(error.as_ref());
            if machine {
                println!("{}", crate::print_mode::startup_failure(&failure));
            } else {
                eprintln!("yawl: {}", failure.message);
                if let Some(hint) = failure.hint {
                    eprintln!("hint: {hint}");
                }
            }
            std::process::exit(failure.code);
        }
    }
}

fn run(args: Vec<String>) -> Result<i32, Box<dyn std::error::Error>> {
    crate::terminal_mode::enable_escape_output();
    let mut cli = parse_args(args).map_err(Failure::usage)?;
    let machine = cli.format.is_machine();
    if cli.help {
        print!("{HELP}");
        return Ok(0);
    }
    if cli.version {
        println!("yawl {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    if cli.print_skill {
        print!("{}", crate::skill_export::text());
        return Ok(0);
    }
    if let Some(target) = &cli.install_skill {
        let paths = crate::skill_export::install(target)?;
        if machine {
            let installed = serde_json::json!({ "installed": paths });
            println!("{}", crate::text::json_line(&installed));
        } else {
            for path in paths {
                println!("installed {}", path.display());
            }
        }
        return Ok(0);
    }
    if cli.doctor {
        crate::install_interrupt_handler()?;
        return Ok(crate::doctor::run()?);
    }
    if cli.check {
        crate::install_interrupt_handler()?;
        return Ok(crate::inspect::check(
            cli.model.as_deref(),
            cli.trust_project,
            cli.refresh,
            machine,
        ));
    }

    let mut config = Config::load().map_err(|error| {
        Error::Config(format!(
            "{error}; run 'yawl --doctor' to diagnose and repair the configuration"
        ))
    })?;
    if let Some(provider) = &cli.login {
        crate::install_interrupt_handler()?;
        match provider.as_str() {
            "openai-codex" | "codex" => crate::provider::codex::login(&config)?,
            _ => {
                return Err(Box::new(Error::Invalid(format!(
                    "unsupported login provider '{provider}'; supported: openai-codex"
                ))));
            }
        }
        return Ok(0);
    }
    let stdin_is_terminal = io::stdin().is_terminal();
    let inspecting = cli.list_tools || cli.list_models || cli.list_sessions || cli.list_agents;
    // Setup also reads project provider definitions, so resolve trust before
    // either explicit setup or first-run onboarding can use the config.
    // Inspection never asks; it lists what the current trust allows.
    if inspecting {
        crate::project_trust::resolve_quietly(&mut config, cli.trust_project)?;
    } else {
        crate::project_trust::resolve(&mut config, cli.trust_project, stdin_is_terminal)?;
    }
    if cli.setup {
        crate::install_interrupt_handler()?;
        crate::onboarding::run(&config)?;
        return Ok(0);
    }
    if let Some(model) = &cli.model {
        config.model = Some(model.clone());
    }
    if cli.list_tools {
        crate::inspect::list_tools(&config, machine);
        return Ok(0);
    }
    if cli.list_models {
        crate::install_interrupt_handler()?;
        return Ok(crate::inspect::list_models(&config, machine, cli.refresh));
    }
    if cli.list_sessions {
        crate::inspect::list_sessions(&config, machine)?;
        return Ok(0);
    }
    if cli.list_agents {
        crate::inspect::list_agents(&config, machine);
        return Ok(0);
    }

    let interactive =
        cli.prompt.is_empty() && stdin_is_terminal && cli.print_only_option().is_none();
    if config.model.is_none() && !config.setup_skipped && interactive {
        crate::install_interrupt_handler()?;
        config = match crate::onboarding::run(&config)? {
            crate::onboarding::SetupOutcome::Configured(config) => *config,
            crate::onboarding::SetupOutcome::Skipped => return Ok(0),
        };
    }
    // A one-shot run reads its prompt before anything is saved, so a missing
    // prompt leaves no session behind.
    let mut timeout = cli.timeout;
    let prompt = if interactive {
        None
    } else {
        if cli.prompt.is_empty()
            && stdin_is_terminal
            && let Some(option) = cli.print_only_option()
        {
            return Err(Box::new(Failure::usage(format!(
                "{option} needs a prompt, such as yawl {option} ... \"TASK\""
            ))));
        }
        let reading_started = std::time::Instant::now();
        let prompt = read_prompt(
            std::mem::take(&mut cli.prompt),
            !stdin_is_terminal && !cli.no_stdin,
            cli.timeout,
        )?;
        // The deadline covers the whole run, including the wait for stdin.
        timeout = timeout.map(|timeout| timeout.saturating_sub(reading_started.elapsed()));
        Some(prompt)
    };
    let preset = cli
        .agent
        .as_deref()
        .map(|name| find_preset(&config, name))
        .transpose()?;
    // An explicit `-m` wins, then the preset's model; otherwise a resumed
    // session continues with the model it last used.
    let explicit_model = cli
        .model
        .clone()
        .or_else(|| preset.as_ref().and_then(|preset| preset.model.clone()));
    let model = explicit_model
        .clone()
        .or_else(|| config.model.clone())
        .ok_or_else(|| {
            Error::Config(
                "no model configured; run 'yawl --setup', or pass --model PROVIDER:MODEL \
                 (see 'yawl --list-models')"
                    .into(),
            )
        })?;
    ensure_resolvable(&config, &model)?;
    let session = if cli.ephemeral {
        Session::memory(ephemeral_id(), &model)
    } else {
        open_session(&config, &cli, &model)?
    };
    let model = match explicit_model {
        Some(model) => model,
        None => session.resumed_model(&config, &model),
    };
    let mut agent = Agent::new(config, model, session);
    if let Some(preset) = &preset {
        agent.apply_preset(preset);
    }
    if let Some(effort) = &cli.reasoning {
        apply_reasoning(&mut agent, effort)?;
    }
    // Building the provider needs no network, but it fails on a missing
    // key or login exactly as the first request would. Failing here keeps
    // the prompt out of a session that could never answer it.
    if !interactive && let Err(error) = crate::provider::resolve(agent.model(), agent.config()) {
        let _ = agent.discard_if_empty();
        return Err(Box::new(error));
    }

    let Some(prompt) = prompt else {
        crate::tui::run(&mut agent)?;
        if agent.discard_if_empty()? {
            return Ok(0);
        }
        println!("yawl --session {}", agent.session_id());
        return Ok(0);
    };

    crate::install_interrupt_handler()?;
    let options = RunOptions {
        format: cli.format,
        timeout,
        ephemeral: cli.ephemeral,
        session_warning: agent.session_warning(),
    };
    run_print_mode(&mut agent, prompt, options)
}

/// The preset `--agent` names, from the same directories subagents use.
fn find_preset(config: &Config, name: &str) -> Result<crate::subagent::AgentPreset, Error> {
    let (presets, _) = crate::subagent::presets::discover_cached(
        config,
        &mut crate::subagent::presets::DiscoveryCache::default(),
    );
    let names = presets
        .iter()
        .map(|preset| preset.name.clone())
        .collect::<Vec<_>>();
    presets
        .into_iter()
        .find(|preset| preset.name == name)
        .ok_or_else(|| {
            Error::Invalid(format!(
                "unknown agent preset '{name}'; available: {} (see 'yawl --list-agents')",
                names.join(", ")
            ))
        })
}

/// Rejects a model whose provider prefix names no configured provider
/// before a session is created for it.
fn ensure_resolvable(config: &Config, model: &str) -> Result<(), Error> {
    if crate::model::is_resolvable(config, model) {
        return Ok(());
    }
    let provider = model
        .split_once(':')
        .map_or(model, |(provider, _)| provider);
    Err(Error::Config(format!(
        "model '{model}' names provider '{provider}', which is not configured; \
         run 'yawl --list-models' to see configured providers"
    )))
}

/// Applies `--reasoning`, checked against what the model accepts.
fn apply_reasoning(agent: &mut Agent, effort: &str) -> Result<(), Error> {
    let accepted = crate::model::reasoning_efforts(agent.config(), agent.model());
    if !accepted.contains(&effort) {
        let expected = if accepted.is_empty() {
            "it accepts no reasoning effort".to_string()
        } else {
            format!("expected {}", accepted.join(", "))
        };
        return Err(Error::Invalid(format!(
            "model '{}' does not support reasoning effort '{effort}'; {expected}",
            agent.model()
        )));
    }
    agent.set_reasoning_effort(Some(effort.to_string()))
}

/// An id for a memory-only session. It never names a file.
fn ephemeral_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("ephemeral-{:x}-{nanos:x}", std::process::id())
}

fn open_session(config: &Config, cli: &Cli, model: &str) -> Result<Session, Error> {
    let cwd = crate::config::working_dir();
    let dirs = config.session_dirs(&cwd);
    select_session(cli, &cwd, &dirs, model)
}

fn select_session(
    cli: &Cli,
    cwd: &Path,
    dirs: &SessionDirs,
    model: &str,
) -> Result<Session, Error> {
    if let Some(id) = &cli.session_id {
        return Session::open_searching(&dirs.search, id);
    }
    if cli.continue_latest
        && let Some(session) = Session::open_latest(&dirs.project)?
    {
        return Ok(session);
    }
    Session::create(&dirs.project, cwd, model)
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let session =
            select_session(&parse(&["-c"]), Path::new("/proj"), &dirs, "test-model").unwrap();
        let messages = session.messages();
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

        let session = select_session(
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

        let session = select_session(&parse(&[]), Path::new("/proj"), &dirs, "test-model").unwrap();
        let messages = session.messages();
        assert!(messages.is_empty());
        assert!(project.join(format!("{}.jsonl", session.id)).exists());

        // Meta-only sessions are not listed until a turn is written.
        let infos = crate::session::list(&project).unwrap();
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

        let session =
            select_session(&parse(&["-c"]), Path::new("/proj"), &dirs, "test-model").unwrap();
        let messages = session.messages();
        assert!(messages.is_empty());
        assert_ne!(session.id, "20260101-000000-0001");
        let _ = std::fs::remove_dir_all(&root);
    }
}
