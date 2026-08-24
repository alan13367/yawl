//! Configuration checks. Each finding names a file or section, states the
//! problem, and carries a structured fix when one is safe to offer.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde_json::{Map, Value, json};

use crate::config::{self, Config};
use crate::provider::codex::{CodexLoginStatus, credential_status};

use super::{Finding, Fix, Paths, Severity};

/// Top-level keys the config loader understands. Anything else is reported
/// as informational; writes preserve it.
const KNOWN_KEYS: &[&str] = &[
    "model",
    "anthropic_base_url",
    "openai_base_url",
    "anthropic_api_key",
    "openai_api_key",
    "max_tokens",
    "reasoning_effort",
    "hide_reasoning",
    "accent_color",
    "status_bar_color",
    "text_box_color",
    "scroll_bar",
    "context_windows",
    "auto_compact",
    "compact_threshold",
    "subagents",
    "max_subagents",
    "subagent_model",
    "subagent_request_budget",
    "subagent_timeout_secs",
    "skill_dirs",
    "providers",
    "setup",
];

pub(super) fn run(paths: &Paths) -> Vec<Finding> {
    let mut findings = Vec::new();
    let global = read_config_file(&paths.global, "global config", true, &mut findings);
    let project = read_config_file(&paths.project, "project config", false, &mut findings);
    if let Some(map) = &global {
        field_checks(&paths.global, "global config", map, paths, &mut findings);
    }
    if let Some(map) = &project {
        field_checks(&paths.project, "project config", map, paths, &mut findings);
    }
    check_backups(paths, &mut findings);
    check_auth(paths, &mut findings);
    // Missing files contribute no values. Broken files suppress merged checks
    // until their file-level finding is repaired.
    let global_usable = global.is_some() || !paths.global.exists();
    let project_usable = project.is_some() || !paths.project.exists();
    if global_usable && project_usable && (global.is_some() || project.is_some()) {
        let empty = Map::new();
        cross_checks(
            paths,
            global.as_ref().unwrap_or(&empty),
            project.as_ref().unwrap_or(&empty),
            &mut findings,
        );
    }
    findings
}

/// Reads one config file and reports file-level problems. `Some(map)` means
/// the file parsed as a JSON object; `None` means missing or broken.
fn read_config_file(
    path: &Path,
    area: &str,
    report_missing: bool,
    findings: &mut Vec<Finding>,
) -> Option<Map<String, Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if report_missing {
                findings.push(info(
                    area,
                    format!(
                        "no {} at {}; run 'yawl --setup' to create one",
                        area,
                        path.display()
                    ),
                ));
            }
            return None;
        }
        Err(source) => {
            findings.push(error(area, format!("{}: {source}", path.display()), None));
            return None;
        }
    };
    let mode = std::fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o777)
        .unwrap_or(0o600);
    if mode != 0o600 {
        findings.push(warning(
            area,
            format!("permissions are {mode:o}, expected 600; the file may hold API keys"),
            Some(Fix::ChmodPrivate {
                path: path.to_path_buf(),
            }),
        ));
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Some(map),
        Ok(_) => {
            findings.push(error(
                area,
                "top-level JSON value must be an object".to_string(),
                Some(Fix::QuarantineFile {
                    path: path.to_path_buf(),
                }),
            ));
            None
        }
        Err(parse_error) => {
            findings.push(error(
                area,
                format!("malformed JSON: {parse_error}"),
                Some(Fix::QuarantineFile {
                    path: path.to_path_buf(),
                }),
            ));
            None
        }
    }
}

fn field_checks(
    path: &Path,
    area: &str,
    map: &Map<String, Value>,
    paths: &Paths,
    findings: &mut Vec<Finding>,
) {
    let value = Value::Object(map.clone());
    if let Err(message) = config::validate_file_shape(&value) {
        match locate_offending_key(map) {
            Some(keys) => findings.push(error(
                area,
                format!(
                    "{message}; removing {} should restore loading",
                    keys.join(".")
                ),
                Some(Fix::RemoveKey {
                    path: path.to_path_buf(),
                    keys,
                }),
            )),
            None => findings.push(error(area, message, None)),
        }
        return;
    }
    semantic_checks(path, area, map, paths, findings);
}

/// Finds the innermost key whose removal makes the file deserialize. The
/// search narrows recursively so one bad field does not cost a whole
/// providers map.
fn locate_offending_key(map: &Map<String, Value>) -> Option<Vec<String>> {
    for key in map.keys() {
        let path = vec![key.clone()];
        if removal_parses(map, &path) {
            return Some(narrow(map, path));
        }
    }
    None
}

fn narrow(map: &Map<String, Value>, path: Vec<String>) -> Vec<String> {
    let nested = super::value_at(&Value::Object(map.clone()), &path)
        .and_then(|value| value.as_object().cloned());
    let Some(children) = nested else {
        return path;
    };
    for child in children.keys() {
        let mut child_path = path.clone();
        child_path.push(child.clone());
        if removal_parses(map, &child_path) {
            return narrow(map, child_path);
        }
    }
    path
}

fn removal_parses(map: &Map<String, Value>, keys: &[String]) -> bool {
    let mut trial = Value::Object(map.clone());
    if !super::remove_nested(&mut trial, keys) {
        return false;
    }
    config::validate_file_shape(&trial).is_ok()
}

/// Value rules mirroring `config::loading`, each with a fix that restores a
/// working default.
fn semantic_checks(
    path: &Path,
    area: &str,
    map: &Map<String, Value>,
    paths: &Paths,
    findings: &mut Vec<Finding>,
) {
    if map.get("max_tokens").and_then(Value::as_u64) == Some(0) {
        findings.push(error(
            area,
            "max_tokens must be a positive integer".into(),
            Some(set(path, ["max_tokens"], json!(config::DEFAULT_MAX_TOKENS))),
        ));
    }
    if let Some(threshold) = map.get("compact_threshold").and_then(Value::as_f64)
        && config::validate_bounded(threshold, 0.1, 0.99, "compact_threshold").is_err()
    {
        findings.push(error(
            area,
            config::bounded_message("compact_threshold", 0.1, 0.99),
            Some(set(
                path,
                ["compact_threshold"],
                json!(config::DEFAULT_COMPACT_THRESHOLD),
            )),
        ));
    }
    if let Some(effort) = map.get("reasoning_effort").and_then(Value::as_str)
        && !matches!(
            effort,
            "default" | "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
        )
    {
        findings.push(error(
            area,
            format!("unsupported reasoning effort '{effort}'"),
            Some(remove(path, ["reasoning_effort"])),
        ));
    }
    if let Some(windows) = map.get("context_windows").and_then(Value::as_object) {
        for (model, window) in windows {
            if window.as_u64() == Some(0) {
                findings.push(error(
                    area,
                    format!("context_windows.{model} must be a positive integer"),
                    Some(remove(path, ["context_windows", model.as_str()])),
                ));
            }
        }
    }
    if let Some(limit) = map.get("max_subagents").and_then(Value::as_u64)
        && config::validate_bounded(limit, 1, 16, "max_subagents").is_err()
    {
        findings.push(error(
            area,
            config::bounded_message("max_subagents", 1, 16),
            Some(set(
                path,
                ["max_subagents"],
                json!(config::DEFAULT_MAX_SUBAGENTS),
            )),
        ));
    }
    if let Some(model) = map.get("subagent_model").and_then(Value::as_str)
        && model.trim().is_empty()
    {
        findings.push(error(
            area,
            "subagent_model must not be empty".into(),
            Some(set(
                path,
                ["subagent_model"],
                json!(config::DEFAULT_SUBAGENT_MODEL),
            )),
        ));
    }
    if let Some(budget) = map.get("subagent_request_budget").and_then(Value::as_u64)
        && config::validate_bounded(
            budget,
            0,
            config::MAX_SUBAGENT_REQUEST_BUDGET as u64,
            "subagent_request_budget",
        )
        .is_err()
    {
        findings.push(error(
            area,
            config::bounded_message(
                "subagent_request_budget",
                0,
                config::MAX_SUBAGENT_REQUEST_BUDGET,
            ),
            Some(set(
                path,
                ["subagent_request_budget"],
                json!(config::DEFAULT_SUBAGENT_REQUEST_BUDGET),
            )),
        ));
    }
    if let Some(timeout) = map.get("subagent_timeout_secs").and_then(Value::as_u64)
        && config::validate_bounded(
            timeout,
            0,
            config::MAX_SUBAGENT_TIMEOUT_SECS,
            "subagent_timeout_secs",
        )
        .is_err()
    {
        findings.push(error(
            area,
            config::bounded_message(
                "subagent_timeout_secs",
                0,
                config::MAX_SUBAGENT_TIMEOUT_SECS,
            ),
            Some(set(
                path,
                ["subagent_timeout_secs"],
                json!(config::DEFAULT_SUBAGENT_TIMEOUT_SECS),
            )),
        ));
    }
    if let Some(marker) = map.get("setup")
        && marker.as_str() != Some("skipped")
    {
        findings.push(error(
            area,
            format!("setup must be \"skipped\" if set, found {marker}"),
            Some(remove(path, ["setup"])),
        ));
    }
    if let Some(providers) = map.get("providers").and_then(Value::as_object) {
        for (name, provider) in providers {
            if let Some(api) = provider.get("api").and_then(Value::as_str)
                && api != "openai-completions"
            {
                findings.push(error(
                    area,
                    format!(
                        "providers.{name} uses unsupported API '{api}'; Yawl supports openai-completions"
                    ),
                    Some(set(
                        path,
                        ["providers", name.as_str(), "api"],
                        json!("openai-completions"),
                    )),
                ));
            }
            if let Some(models) = provider.get("models").and_then(Value::as_array) {
                // Findings are applied in report order. Remove higher indexes
                // first so earlier removals cannot shift later targets.
                for (index, model) in models.iter().enumerate().rev() {
                    if model.get("contextWindow").and_then(Value::as_u64) == Some(0) {
                        findings.push(error(
                            area,
                            format!(
                                "providers.{name}.models.{} contextWindow must be a positive integer",
                                model.get("id").and_then(Value::as_str).unwrap_or("?")
                            ),
                            Some(Fix::RemoveKey {
                                path: path.to_path_buf(),
                                keys: vec![
                                    "providers".into(),
                                    name.clone(),
                                    "models".into(),
                                    index.to_string(),
                                ],
                            }),
                        ));
                    }
                }
            }
        }
    }
    check_env_references(path, area, map, findings);
    check_skill_dirs(path, area, map, paths, findings);

    let unknown = map
        .keys()
        .filter(|key| !KNOWN_KEYS.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        findings.push(info(
            area,
            format!(
                "unknown key(s) ignored by this version, kept on write: {}",
                unknown.join(", ")
            ),
        ));
    }
}

/// Reports stored values that reference environment variables which are not
/// set, since requests would fail at resolution time.
fn check_env_references(
    path: &Path,
    area: &str,
    map: &Map<String, Value>,
    findings: &mut Vec<Finding>,
) {
    let mut check = |keys: Vec<&str>, value: &str| {
        let names = match referenced_variables(value) {
            Ok(names) => names,
            Err(message) => {
                findings.push(error(
                    area,
                    format!("{} has {message}", keys.join(".")),
                    None,
                ));
                return;
            }
        };
        for name in names {
            if std::env::var(&name).is_err() {
                findings.push(error(
                    area,
                    format!("{} references {name}, which is not set", keys.join(".")),
                    Some(Fix::RemoveKey {
                        path: path.to_path_buf(),
                        keys: keys.iter().map(|key| (*key).to_string()).collect(),
                    }),
                ));
            }
        }
    };
    for key in ["anthropic_api_key", "openai_api_key"] {
        if let Some(value) = map.get(key).and_then(Value::as_str) {
            check(vec![key], value);
        }
    }
    if let Some(providers) = map.get("providers").and_then(Value::as_object) {
        for (name, provider) in providers {
            if let Some(value) = provider.get("api_key").and_then(Value::as_str) {
                check(vec!["providers", name, "api_key"], value);
            }
            if let Some(headers) = provider.get("headers").and_then(Value::as_object) {
                for (header, value) in headers {
                    if let Some(value) = value.as_str() {
                        check(vec!["providers", name, "headers", header], value);
                    }
                }
            }
        }
    }
}

/// Extracts references with the same scan rules as
/// `config::resolve_config_value`. Escaped `$$` and `$!` pairs are literals.
fn referenced_variables(value: &str) -> Result<Vec<String>, &'static str> {
    if value.starts_with('!') {
        return Err(
            "a value beginning with '!', which is unsupported; use an environment variable",
        );
    }
    let chars = value.chars().collect::<Vec<_>>();
    let mut names = Vec::new();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] != '$' {
            index += 1;
            continue;
        }
        let Some(next) = chars.get(index + 1).copied() else {
            break;
        };
        if matches!(next, '$' | '!') {
            index += 2;
            continue;
        }
        let (name, next_index) = if next == '{' {
            let Some(relative_end) = chars[index + 2..]
                .iter()
                .position(|character| *character == '}')
            else {
                return Err("an unterminated environment variable reference");
            };
            let end = index + 2 + relative_end;
            (chars[index + 2..end].iter().collect::<String>(), end + 1)
        } else {
            let end = chars[index + 1..]
                .iter()
                .position(|character| !(character.is_ascii_alphanumeric() || *character == '_'))
                .map_or(chars.len(), |relative| index + 1 + relative);
            if end == index + 1 {
                index += 1;
                continue;
            }
            (chars[index + 1..end].iter().collect::<String>(), end)
        };
        if !names.contains(&name) {
            names.push(name);
        }
        index = next_index;
    }
    Ok(names)
}

fn check_skill_dirs(
    path: &Path,
    area: &str,
    map: &Map<String, Value>,
    paths: &Paths,
    findings: &mut Vec<Finding>,
) {
    let Some(dirs) = map.get("skill_dirs").and_then(Value::as_array) else {
        return;
    };
    // `expand_home_path` accepts the config home (`~/.yawl`), not `~`.
    let config_home = paths.global.parent();
    let mut missing = Vec::new();
    let mut kept = Vec::new();
    for dir in dirs.iter().filter_map(Value::as_str) {
        let expanded = config_home.map_or_else(
            || Path::new(dir).to_path_buf(),
            |home| config::expand_home_path(dir, home),
        );
        if expanded.is_dir() {
            kept.push(json!(dir));
        } else {
            missing.push(dir.to_string());
        }
    }
    if missing.is_empty() {
        return;
    }
    findings.push(warning(
        area,
        format!("skill_dirs entries do not exist: {}", missing.join(", ")),
        Some(Fix::SetValue {
            path: path.to_path_buf(),
            keys: vec!["skill_dirs".into()],
            value: Value::Array(kept),
        }),
    ));
}

fn check_backups(paths: &Paths, findings: &mut Vec<Finding>) {
    let Some(dir) = paths.global.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut backups = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("config.json.bak-"))
        .collect::<Vec<_>>();
    if backups.is_empty() {
        return;
    }
    backups.sort();
    let newest_valid = backups
        .iter()
        .rev()
        .find(|name| is_restorable_backup(&dir.join(name)));
    findings.push(Finding {
        severity: Severity::Info,
        area: "backups".into(),
        message: format!(
            "{} backup file(s) beside the config: {}",
            backups.len(),
            backups.join(", ")
        ),
        fix: newest_valid.map(|name| Fix::RestoreBackup {
            from: dir.join(name),
            to: paths.global.clone(),
        }),
    });
}

fn is_restorable_backup(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    value.is_object()
        && path
            .parent()
            .is_some_and(|home| config::validate_file_value(&value, home).is_ok())
}

fn check_auth(paths: &Paths, findings: &mut Vec<Finding>) {
    let Ok(text) = std::fs::read_to_string(&paths.auth) else {
        return;
    };
    let parsed = serde_json::from_str::<Value>(&text);
    let root = match parsed {
        Ok(value) => value,
        Err(error) => {
            findings.push(warning(
                "auth.json",
                format!("malformed JSON: {error}; run 'yawl --login openai-codex' to re-login"),
                None,
            ));
            return;
        }
    };
    let Some(credential) = root.get("openai-codex") else {
        return;
    };
    let Some(object) = credential.as_object() else {
        findings.push(warning(
            "auth.json",
            "the openai-codex entry must be an object; run 'yawl --login openai-codex'".into(),
            None,
        ));
        return;
    };
    if object
        .get("refresh")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        findings.push(warning(
            "auth.json",
            "the Codex credential has no refresh token; run 'yawl --login openai-codex'".into(),
            None,
        ));
        return;
    }
    if let Some(expires) = object.get("expires").and_then(Value::as_u64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if expires <= now {
            findings.push(info(
                "auth.json",
                "the Codex access token is expired; it refreshes on the next request".into(),
            ));
        }
    }
}

/// Checks that need the merged effective config. Only runs when every
/// existing file parses, so a broken file never hides behind downstream
/// confusion.
fn cross_checks(
    paths: &Paths,
    global_map: &Map<String, Value>,
    project_map: &Map<String, Value>,
    findings: &mut Vec<Finding>,
) {
    let overridden = project_map
        .keys()
        .filter(|key| global_map.contains_key(*key))
        .cloned()
        .collect::<Vec<_>>();
    if !overridden.is_empty() {
        findings.push(info(
            "project config",
            format!("project values override: {}", overridden.join(", ")),
        ));
    }

    let Some(home_dir) = paths.global.parent().map(Path::to_path_buf) else {
        return;
    };
    let Ok(config) = Config::load_from(
        home_dir,
        paths
            .project
            .parent()
            .unwrap_or(&paths.project)
            .to_path_buf(),
    ) else {
        return;
    };
    if config.setup_skipped {
        let path = if project_map.contains_key("setup") {
            paths.project.clone()
        } else {
            paths.global.clone()
        };
        findings.push(Finding {
            severity: Severity::Info,
            area: "setup".into(),
            message: "setup was skipped; run 'yawl --setup' to configure a model".into(),
            fix: Some(remove(&path, ["setup"])),
        });
    }
    let Some(model) = config.model.clone() else {
        findings.push(info(
            "model",
            "no model configured; run 'yawl --setup'".into(),
        ));
        return;
    };
    check_model_routing(&model, &config, findings);
}

fn check_model_routing(model: &str, config: &Config, findings: &mut Vec<Finding>) {
    let (name, _) = model.split_once(':').unwrap_or(("", model));
    if let Some(provider) = config.providers.get(name) {
        check_custom_provider(name, provider, findings);
        return;
    }
    match name {
        "anthropic" => check_builtin_key(
            "Anthropic",
            "ANTHROPIC_API_KEY",
            config.anthropic_api_key.as_deref(),
            Severity::Error,
            findings,
        ),
        "openai" => check_builtin_key(
            "OpenAI",
            "OPENAI_API_KEY",
            config.openai_api_key.as_deref(),
            Severity::Warning,
            findings,
        ),
        "openai-codex" => {
            if credential_status(config) == CodexLoginStatus::Missing {
                findings.push(warning(
                    "auth.json",
                    "OpenAI Codex is not logged in; run 'yawl --login openai-codex'".into(),
                    None,
                ));
            }
        }
        // Bare model names need no provider entry.
        "" => {}
        other => findings.push(error(
            "model",
            format!(
                "model '{model}' names provider '{other}', which is not configured; run 'yawl --setup'"
            ),
            None,
        )),
    }
}

fn check_custom_provider(
    name: &str,
    provider: &crate::config::ProviderConfig,
    findings: &mut Vec<Finding>,
) {
    if provider.base_url.trim().is_empty() {
        findings.push(error(
            "model",
            format!("provider '{name}' has no base_url; run 'yawl --setup' or edit the config"),
            None,
        ));
    }
}

fn check_builtin_key(
    label: &str,
    env_name: &str,
    stored: Option<&str>,
    missing_severity: Severity,
    findings: &mut Vec<Finding>,
) {
    let env_set = std::env::var(env_name).is_ok_and(|value| !value.trim().is_empty());
    if env_set {
        return;
    }
    match stored {
        Some(_) => {}
        None => findings.push(Finding {
            severity: missing_severity,
            area: "model".into(),
            message: format!(
                "no {label} API key; set {env_name} or add {} via 'yawl --setup'",
                env_name.to_lowercase()
            ),
            fix: None,
        }),
    }
}

fn error(area: &str, message: String, fix: Option<Fix>) -> Finding {
    Finding {
        severity: Severity::Error,
        area: area.into(),
        message,
        fix,
    }
}

fn warning(area: &str, message: String, fix: Option<Fix>) -> Finding {
    Finding {
        severity: Severity::Warning,
        area: area.into(),
        message,
        fix,
    }
}

fn info(area: &str, message: String) -> Finding {
    Finding {
        severity: Severity::Info,
        area: area.into(),
        message,
        fix: None,
    }
}

fn set<const N: usize>(path: &Path, keys: [&str; N], value: Value) -> Fix {
    Fix::SetValue {
        path: path.to_path_buf(),
        keys: keys.iter().map(|key| (*key).to_string()).collect(),
        value,
    }
}

fn remove<const N: usize>(path: &Path, keys: [&str; N]) -> Fix {
    Fix::RemoveKey {
        path: path.to_path_buf(),
        keys: keys.iter().map(|key| (*key).to_string()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct TestDirs {
        root: PathBuf,
        global: PathBuf,
        project: PathBuf,
        auth: PathBuf,
    }

    impl TestDirs {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("yawl-doctor-{}-{nonce}-{name}", std::process::id()));
            let dirs = Self {
                global: root.join("home/.yawl/config.json"),
                project: root.join("project/.yawl/config.json"),
                auth: root.join("home/.yawl/auth.json"),
                root,
            };
            fs::create_dir_all(dirs.global.parent().unwrap()).unwrap();
            fs::create_dir_all(dirs.project.parent().unwrap()).unwrap();
            dirs
        }

        fn paths(&self) -> Paths {
            Paths {
                global: self.global.clone(),
                project: self.project.clone(),
                auth: self.auth.clone(),
            }
        }

        fn write_global(&self, text: &str) {
            fs::write(&self.global, text).unwrap();
        }
    }

    impl Drop for TestDirs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn errors(findings: &[Finding]) -> Vec<&Finding> {
        findings
            .iter()
            .filter(|finding| finding.severity == Severity::Error)
            .collect()
    }

    #[test]
    fn healthy_config_produces_no_error_findings() {
        let dirs = TestDirs::new("healthy");
        dirs.write_global(r#"{"model":"ollama:llama4"}"#);

        let findings = run(&dirs.paths());

        assert!(errors(&findings).is_empty(), "findings: {findings:?}");
    }

    #[test]
    fn missing_global_config_is_informational() {
        let dirs = TestDirs::new("missing");

        let findings = run(&dirs.paths());

        assert!(errors(&findings).is_empty());
        assert!(
            findings
                .iter()
                .any(|finding| finding.message.contains("no global config"))
        );
    }

    #[test]
    fn malformed_json_offers_quarantine() {
        let dirs = TestDirs::new("malformed");
        dirs.write_global("{\"model\": ");

        let findings = run(&dirs.paths());
        let error_findings = errors(&findings);
        let finding = error_findings
            .iter()
            .find(|finding| finding.message.contains("malformed JSON"))
            .expect("malformed JSON should be an error");

        assert!(matches!(finding.fix, Some(Fix::QuarantineFile { .. })));
    }

    #[test]
    fn wrong_type_is_attributed_to_its_key() {
        let dirs = TestDirs::new("wrong-type");
        dirs.write_global(r#"{"max_tokens":"8192"}"#);

        let findings = run(&dirs.paths());
        let error_findings = errors(&findings);
        let finding = error_findings
            .iter()
            .find(|finding| finding.message.contains("max_tokens"))
            .expect("the typed field should be flagged");

        match &finding.fix {
            Some(Fix::RemoveKey { keys, .. }) => assert_eq!(keys, &["max_tokens".to_string()]),
            other => panic!("expected RemoveKey, got {other:?}"),
        }
    }

    #[test]
    fn bad_provider_field_is_narrowed_to_the_provider() {
        let dirs = TestDirs::new("provider-field");
        dirs.write_global(
            r#"{"providers":{"omlx":{"api_key":5},"ollama":{"base_url":"http://127.0.0.1:11434/v1"}}}"#,
        );

        let findings = run(&dirs.paths());
        let error_findings = errors(&findings);
        let finding = error_findings
            .iter()
            .find(|finding| finding.fix.is_some())
            .expect("the bad provider should carry a fix");

        match &finding.fix {
            Some(Fix::RemoveKey { keys, .. }) => {
                assert_eq!(keys[0], "providers");
                assert_eq!(keys[1], "omlx");
            }
            other => panic!("expected RemoveKey, got {other:?}"),
        }
    }

    #[test]
    fn semantic_rules_reset_defaults() {
        let dirs = TestDirs::new("semantic");
        dirs.write_global(
            r#"{"max_tokens":0,"compact_threshold":1.5,"reasoning_effort":"extreme","setup":"later"}"#,
        );

        let findings = run(&dirs.paths());
        let messages = errors(&findings)
            .iter()
            .map(|finding| finding.message.as_str())
            .collect::<Vec<_>>();
        for expected in [
            "max_tokens must be a positive integer",
            "compact_threshold must be between 0.1 and 0.99",
            "unsupported reasoning effort 'extreme'",
            "setup must be",
        ] {
            assert!(
                messages.iter().any(|message| message.contains(expected)),
                "missing {expected} in {messages:?}"
            );
        }
        assert!(
            errors(&findings)
                .iter()
                .all(|finding| finding.fix.is_some())
        );
    }

    #[test]
    fn unknown_provider_in_model_is_an_error() {
        let dirs = TestDirs::new("unknown-provider");
        dirs.write_global(r#"{"model":"ghost:haunt"}"#);

        let findings = run(&dirs.paths());

        assert!(
            errors(&findings)
                .iter()
                .any(|finding| finding.message.contains("provider 'ghost'"))
        );
    }

    #[test]
    fn model_referencing_provider_without_base_url_fails() {
        let dirs = TestDirs::new("empty-base-url");
        dirs.write_global(r#"{"model":"empty:m","providers":{"empty":{}}}"#);

        let findings = run(&dirs.paths());

        assert!(
            errors(&findings)
                .iter()
                .any(|finding| finding.message.contains("no base_url"))
        );
    }

    #[test]
    fn escaped_environment_markers_are_not_reported_as_missing() {
        let dirs = TestDirs::new("escaped-env");
        let map = serde_json::from_value::<Value>(json!({
            "openai_api_key": "$$YAWL_DOCTOR_ESCAPED_MISSING-$!literal"
        }))
        .unwrap()
        .as_object()
        .unwrap()
        .clone();
        let mut findings = Vec::new();

        check_env_references(&dirs.global, "global config", &map, &mut findings);

        assert!(findings.is_empty(), "findings: {findings:?}");
    }

    #[test]
    fn unbraced_environment_reference_stops_before_suffix() {
        let dirs = TestDirs::new("env-suffix");
        let name = format!(
            "YAWL_DOCTOR_MISSING_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        assert!(std::env::var_os(&name).is_none());
        let map = serde_json::from_value::<Value>(json!({
            "openai_api_key": format!("${name}-suffix")
        }))
        .unwrap()
        .as_object()
        .unwrap()
        .clone();
        let mut findings = Vec::new();

        check_env_references(&dirs.global, "global config", &map, &mut findings);

        assert_eq!(findings.len(), 1);
        assert!(
            findings[0]
                .message
                .contains(&format!("references {name}, which is not set")),
            "finding: {:?}",
            findings[0]
        );
    }

    #[test]
    fn missing_skill_directories_warn_with_a_fix() {
        let dirs = TestDirs::new("skill-dirs");
        let kept = dirs.root.join("kept-skills");
        fs::create_dir_all(&kept).unwrap();
        dirs.write_global(
            &serde_json::json!({
                "skill_dirs": [kept.display().to_string(), dirs.root.join("missing-skills").display().to_string()]
            })
            .to_string(),
        );

        let findings = run(&dirs.paths());
        let finding = findings
            .iter()
            .find(|finding| {
                finding.severity == Severity::Warning && finding.message.contains("skill_dirs")
            })
            .expect("missing skill dirs should warn");

        match &finding.fix {
            Some(Fix::SetValue { value, .. }) => {
                assert_eq!(value.as_array().map(Vec::len), Some(1));
            }
            other => panic!("expected SetValue, got {other:?}"),
        }
    }

    #[test]
    fn tilde_skill_directory_uses_the_home_above_dot_yawl() {
        let dirs = TestDirs::new("tilde-skill-dir");
        fs::create_dir_all(dirs.root.join("home/skills")).unwrap();
        dirs.write_global(r#"{"skill_dirs":["~/skills"]}"#);

        let findings = run(&dirs.paths());

        assert!(
            !findings
                .iter()
                .any(|finding| finding.message.contains("skill_dirs entries do not exist")),
            "findings: {findings:?}"
        );
    }

    #[test]
    fn project_only_config_runs_cross_checks() {
        let dirs = TestDirs::new("project-only");
        fs::write(&dirs.project, r#"{"model":"ghost:haunt"}"#).unwrap();

        let findings = run(&dirs.paths());

        assert!(
            errors(&findings)
                .iter()
                .any(|finding| finding.message.contains("provider 'ghost'")),
            "findings: {findings:?}"
        );
    }

    #[test]
    fn only_valid_bak_files_are_offered_for_restore() {
        let dirs = TestDirs::new("restorable-backups");
        dirs.write_global(r#"{"model":"ollama:current"}"#);
        let dir = dirs.global.parent().unwrap();
        let valid = dir.join("config.json.bak-1");
        fs::write(&valid, r#"{"model":"ollama:backup"}"#).unwrap();
        fs::write(dir.join("config.json.bak-8"), "{not json").unwrap();
        fs::write(dir.join("config.json.bak-9"), r#"{"max_tokens":0}"#).unwrap();
        fs::write(
            dir.join("config.json.invalid-10"),
            r#"{"model":"ollama:quarantined"}"#,
        )
        .unwrap();

        let findings = run(&dirs.paths());
        let restore_source = findings.iter().find_map(|finding| match &finding.fix {
            Some(Fix::RestoreBackup { from, .. }) => Some(from),
            _ => None,
        });

        assert_eq!(restore_source, Some(&valid));
    }

    #[test]
    fn project_overrides_and_setup_marker_are_informational() {
        let dirs = TestDirs::new("overrides");
        dirs.write_global(r#"{"max_tokens":2048,"setup":"skipped"}"#);
        fs::write(&dirs.project, r#"{"max_tokens":4096}"#).unwrap();

        let findings = run(&dirs.paths());

        assert!(errors(&findings).is_empty());
        assert!(findings.iter().any(|finding| {
            finding
                .message
                .contains("project values override: max_tokens")
        }));
        let marker = findings
            .iter()
            .find(|finding| finding.message.contains("setup was skipped"))
            .expect("the skip marker should be reported");
        assert!(matches!(marker.fix, Some(Fix::RemoveKey { .. })));
    }
}
