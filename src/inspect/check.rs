//! `yawl --check`: whether a turn could start right now, and if not, what
//! to fix. It never prompts, so another agent can run it before delegating.

use serde_json::{Value, json};

use crate::config::Config;
use crate::exit_status;

use super::models;

#[derive(Debug)]
struct Problem {
    severity: &'static str,
    area: String,
    message: String,
    fix: Option<String>,
}

impl Problem {
    fn error(area: &str, message: impl Into<String>, fix: Option<&str>) -> Self {
        Self {
            severity: "error",
            area: area.into(),
            message: message.into(),
            fix: fix.map(str::to_string),
        }
    }

    fn warning(area: &str, message: impl Into<String>, fix: Option<&str>) -> Self {
        Self {
            severity: "warning",
            ..Self::error(area, message, fix)
        }
    }
}

/// What `--check` found about the model a turn would use.
#[derive(Debug, Default)]
struct Readiness {
    model: Option<String>,
    provider: Option<String>,
    capabilities: Option<Value>,
    project_trusted: Option<bool>,
    probe: Option<Value>,
}

/// Runs every check and prints the report. Returns 0 when ready and the
/// configuration exit code otherwise.
pub(crate) fn run(
    model_override: Option<&str>,
    trust_project: bool,
    refresh: bool,
    machine: bool,
) -> i32 {
    let mut problems = Vec::new();
    match crate::doctor::diagnose() {
        Ok(findings) => problems.extend(findings.into_iter().map(|finding| {
            Problem {
                severity: match finding.severity {
                    crate::doctor::Severity::Error => "error",
                    crate::doctor::Severity::Warning => "warning",
                    crate::doctor::Severity::Info => "info",
                },
                area: finding.area,
                message: finding.message,
                fix: finding
                    .fix
                    .map(|fix| format!("{} (run 'yawl --doctor')", fix.describe())),
            }
        })),
        Err(error) => problems.push(Problem::error("doctor", error.to_string(), None)),
    }
    let mut readiness = Readiness::default();
    match Config::load() {
        Ok(mut config) => check_config(
            &mut config,
            model_override,
            trust_project,
            refresh,
            &mut readiness,
            &mut problems,
        ),
        Err(error) => problems.push(Problem::error(
            "config",
            error.to_string(),
            Some("run 'yawl --doctor' to repair the configuration"),
        )),
    }
    let ready = !problems.iter().any(|problem| problem.severity == "error");
    if machine {
        println!(
            "{}",
            crate::text::json_line(&report_json(ready, &readiness, &problems))
        );
    } else {
        print_text(ready, &readiness, &problems);
    }
    if ready { 0 } else { exit_status::CONFIG }
}

fn check_config(
    config: &mut Config,
    model_override: Option<&str>,
    trust_project: bool,
    refresh: bool,
    readiness: &mut Readiness,
    problems: &mut Vec<Problem>,
) {
    match crate::project_trust::resolve_quietly(config, trust_project) {
        Ok(pending) if pending.is_empty() => {}
        Ok(pending) => problems.push(Problem::warning(
            "project",
            format!(
                "this project's {} are ignored until the project is trusted",
                pending.join(", ")
            ),
            Some("pass --trust-project if the user trusts this repository"),
        )),
        Err(error) => problems.push(Problem::error("project", error.to_string(), None)),
    }
    readiness.project_trusted = Some(config.project_skills_trusted());
    let Some(model) = model_override
        .map(str::to_string)
        .or_else(|| config.model.clone())
    else {
        problems.push(Problem::error(
            "model",
            "no model is configured",
            Some("run 'yawl --setup', or pass --model PROVIDER:MODEL (see 'yawl --list-models')"),
        ));
        return;
    };
    let provider = models::provider_name(&model);
    readiness.model = Some(model.clone());
    readiness.provider = Some(provider.clone());
    if !crate::model::is_resolvable(config, &model) {
        problems.push(Problem::error(
            "model",
            format!("'{model}' names provider '{provider}', which is not configured"),
            Some("run 'yawl --list-models' to see configured providers"),
        ));
        return;
    }
    readiness.capabilities = Some(models::capabilities(config, &model));
    // Building the provider needs no network but fails exactly as a turn
    // would on a missing key, login, or invalid endpoint.
    if let Err(error) = crate::provider::resolve(&model, config) {
        let fix = match provider.as_str() {
            "openai-codex" => "run 'yawl --login openai-codex'",
            "anthropic" => "set ANTHROPIC_API_KEY",
            "openai" => "set OPENAI_API_KEY",
            _ => "run 'yawl --setup' or fix the provider in ~/.yawl/config.json",
        };
        problems.push(Problem::error("provider", error.to_string(), Some(fix)));
        return;
    }
    if refresh {
        readiness.probe = Some(probe(config, &model, &provider, problems));
    }
}

/// Asks the model's provider for its models, which proves the endpoint and
/// credential work without spending a completion.
fn probe(config: &Config, model: &str, provider: &str, problems: &mut Vec<Problem>) -> Value {
    let Some(status) = models::providers(config)
        .into_iter()
        .find(|status| status.name == provider)
    else {
        return json!({"reachable": Value::Null, "error": "provider cannot be probed"});
    };
    let base_url = status.base_url.clone().unwrap_or_default();
    match models::list_provider_models(config, &status.id, &base_url) {
        Ok(listed) => {
            let bare = crate::model::ModelTarget::parse(model, config).model();
            let listed_model = listed.iter().any(|candidate| candidate.id == bare);
            if !listed_model {
                problems.push(Problem::warning(
                    "model",
                    format!("{provider} does not list '{bare}'; requests may fail"),
                    Some("run 'yawl --list-models --refresh' to see what it serves"),
                ));
            }
            json!({"reachable": true, "model_listed": listed_model, "listed_models": listed.len()})
        }
        Err(error) => {
            let message = error.to_string();
            let failure = exit_status::classify_error(&error);
            problems.push(Problem::error(
                "provider",
                format!("could not reach {provider}: {message}"),
                failure.hint,
            ));
            json!({"reachable": false, "error": message})
        }
    }
}

fn report_json(ready: bool, readiness: &Readiness, problems: &[Problem]) -> Value {
    json!({
        "ready": ready,
        "version": env!("CARGO_PKG_VERSION"),
        "model": readiness.model,
        "provider": readiness.provider,
        "capabilities": readiness.capabilities,
        "project": {
            "root": crate::trust::project_root(&crate::config::working_dir()),
            "trusted": readiness.project_trusted,
        },
        "probe": readiness.probe,
        "problems": problems.iter().map(|problem| json!({
            "severity": problem.severity,
            "area": problem.area,
            "message": problem.message,
            "fix": problem.fix,
        })).collect::<Vec<_>>(),
    })
}

fn print_text(ready: bool, readiness: &Readiness, problems: &[Problem]) {
    let safe = |text: &str| crate::text::sanitize_line(text, crate::text::Tabs::Space);
    match (&readiness.model, ready) {
        (Some(model), true) => println!("ready: {}", safe(model)),
        (Some(model), false) => println!("not ready: {}", safe(model)),
        (None, _) => println!("not ready"),
    }
    if let Some(probe) = &readiness.probe
        && probe["reachable"] == true
    {
        let listed = if probe["model_listed"] == true {
            "lists the model"
        } else {
            "does not list the model"
        };
        println!("  provider reachable and {listed}");
    }
    for problem in problems {
        let fix = problem
            .fix
            .as_deref()
            .map(|fix| format!(" [fix: {}]", safe(fix)))
            .unwrap_or_default();
        println!(
            "  {} {}: {}{fix}",
            problem.severity,
            safe(&problem.area),
            safe(&problem.message)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_unknown_models_are_errors_with_fixes() {
        let mut problems = Vec::new();
        let mut readiness = Readiness::default();
        let mut config = Config::test_default();
        check_config(
            &mut config,
            None,
            false,
            false,
            &mut readiness,
            &mut problems,
        );
        let problem = problems.last().expect("no model is a problem");
        assert_eq!(
            (problem.severity, problem.area.as_str()),
            ("error", "model")
        );
        assert!(problem.fix.as_deref().unwrap().contains("--setup"));

        problems.clear();
        check_config(
            &mut config,
            Some("nowhere:model"),
            false,
            false,
            &mut readiness,
            &mut problems,
        );
        let problem = problems.last().expect("unknown provider is a problem");
        assert!(problem.message.contains("'nowhere'"));
        assert_eq!(readiness.provider.as_deref(), Some("nowhere"));
    }

    #[test]
    fn report_json_is_ready_only_without_errors() {
        let readiness = Readiness {
            model: Some("ollama:qwen".into()),
            ..Readiness::default()
        };
        let warning = [Problem::warning("project", "ignored", None)];
        let value = report_json(true, &readiness, &warning);
        assert_eq!(value["ready"], true);
        assert_eq!(value["model"], "ollama:qwen");
        assert_eq!(value["problems"][0]["severity"], "warning");
    }
}
