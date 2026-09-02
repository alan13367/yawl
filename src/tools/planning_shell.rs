//! Conservative command gate for read-only planning inspection.

use serde_json::Value;

const ALLOWED_COMMANDS: &[&str] = &[
    "basename", "cat", "cut", "dirname", "du", "git", "grep", "head", "ls", "pwd", "readlink",
    "realpath", "rg", "sed", "stat", "tail", "tr", "wc",
];

pub(super) fn prepare(args: &Value) -> Result<Value, String> {
    if args.get("background").is_some() || args.get("name").is_some() {
        return Err("background commands are unavailable during planning".into());
    }
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .ok_or_else(|| "missing required string argument 'command'".to_string())?;
    let commands = parse_pipeline(command)?;
    for words in &commands {
        validate_command(words)?;
    }
    let mut prepared = args.clone();
    prepared["command"] = Value::String(render_pipeline(&commands));
    Ok(prepared)
}

fn validate_command(words: &[String]) -> Result<(), String> {
    let Some(name) = words.first().map(String::as_str) else {
        return Err("planning shell commands cannot be empty".into());
    };
    if !ALLOWED_COMMANDS.contains(&name) {
        return Err(format!(
            "command '{name}' is unavailable during planning; allowed inspection commands: {}",
            ALLOWED_COMMANDS.join(", ")
        ));
    }
    match name {
        "rg" => reject_options(words, &["--pre", "--hostname-bin"]),
        "git" => validate_git(words),
        "sed" => validate_sed(words),
        _ => Ok(()),
    }
}

fn reject_options(words: &[String], denied: &[&str]) -> Result<(), String> {
    if let Some(option) = words.iter().skip(1).find(|word| {
        let key = word.split_once('=').map_or(word.as_str(), |(key, _)| key);
        denied.iter().any(|denied| {
            key == *denied
                || if denied.starts_with("--") {
                    key.starts_with("--") && key.len() > 2 && denied.starts_with(key)
                } else {
                    key.starts_with(denied)
                }
        })
    }) {
        return Err(format!(
            "option '{option}' is unavailable in the planning shell"
        ));
    }
    Ok(())
}

fn validate_git(words: &[String]) -> Result<(), String> {
    const SUBCOMMANDS: &[&str] = &[
        "blame",
        "describe",
        "diff",
        "grep",
        "log",
        "ls-files",
        "ls-tree",
        "rev-parse",
        "shortlog",
        "show",
        "status",
    ];
    let Some(subcommand) = words.get(1).map(String::as_str) else {
        return Err("git requires an allowed read-only subcommand during planning".into());
    };
    if !SUBCOMMANDS.contains(&subcommand) {
        return Err(format!(
            "git subcommand '{subcommand}' is unavailable during planning; allowed subcommands: {}",
            SUBCOMMANDS.join(", ")
        ));
    }
    reject_options(
        words,
        &[
            "--exec",
            "--ext-diff",
            "--open-files-in-pager",
            "--output",
            "--textconv",
        ],
    )?;
    if let Some(option) = words
        .iter()
        .find(|word| word.as_str() == "-O" || word.starts_with("-O"))
    {
        return Err(format!(
            "option '{option}' is unavailable in the planning shell"
        ));
    }
    Ok(())
}

fn validate_sed(words: &[String]) -> Result<(), String> {
    let mut script = None;
    for word in words.iter().skip(1) {
        match word.as_str() {
            "-n" | "--quiet" | "--silent" | "-E" | "-r" | "--regexp-extended" => {}
            option if option.starts_with('-') => {
                return Err(format!(
                    "sed option '{option}' is unavailable in the planning shell"
                ));
            }
            value if script.is_none() => script = Some(value),
            _ => {}
        }
    }
    let Some(script) = script else {
        return Err("sed requires a print-only script during planning".into());
    };
    let range = script
        .strip_suffix('p')
        .ok_or_else(|| "sed is limited to print-only ranges such as '1,120p'".to_string())?;
    let mut bounds = range.split(',');
    let valid_bound = |bound: &str| {
        !bound.is_empty() && (bound == "$" || bound.chars().all(|ch| ch.is_ascii_digit()))
    };
    if range.is_empty() || !bounds.by_ref().all(valid_bound) || range.split(',').count() > 2 {
        return Err("sed is limited to print-only ranges such as '1,120p'".into());
    }
    Ok(())
}

fn parse_pipeline(command: &str) -> Result<Vec<Vec<String>>, String> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = Quote::None;
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        match quote {
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else if matches!(ch, '\n' | '\r') {
                    return Err("multiline shell commands are unavailable during planning".into());
                } else {
                    word.push(ch);
                }
            }
            Quote::Double => match ch {
                '"' => quote = Quote::None,
                '`' | '$' => {
                    return Err(
                        "command substitution and expansion are unavailable during planning".into(),
                    );
                }
                '\\' => word.push(
                    chars
                        .next()
                        .ok_or_else(|| "planning shell command ends with an escape".to_string())?,
                ),
                '\n' | '\r' => {
                    return Err("multiline shell commands are unavailable during planning".into());
                }
                _ => word.push(ch),
            },
            Quote::None => match ch {
                '\'' => quote = Quote::Single,
                '"' => quote = Quote::Double,
                '\\' => word.push(
                    chars
                        .next()
                        .ok_or_else(|| "planning shell command ends with an escape".to_string())?,
                ),
                ' ' | '\t' => push_word(&mut words, &mut word),
                '|' => {
                    if chars.peek() == Some(&'|') {
                        return Err(
                            "logical shell operators are unavailable during planning".into()
                        );
                    }
                    push_word(&mut words, &mut word);
                    if words.is_empty() {
                        return Err("planning shell pipeline contains an empty command".into());
                    }
                    commands.push(std::mem::take(&mut words));
                }
                ';' | '&' => {
                    return Err("shell command chaining is unavailable during planning".into());
                }
                '<' | '>' => {
                    return Err("shell redirection is unavailable during planning".into());
                }
                '`' | '$' | '(' | ')' => {
                    return Err(
                        "command substitution and expansion are unavailable during planning".into(),
                    );
                }
                '#' => return Err("shell comments are unavailable during planning".into()),
                '\n' | '\r' => {
                    return Err("multiline shell commands are unavailable during planning".into());
                }
                _ => word.push(ch),
            },
        }
    }
    if quote != Quote::None {
        return Err("planning shell command contains an unterminated quote".into());
    }
    push_word(&mut words, &mut word);
    if words.is_empty() {
        return Err("planning shell pipeline contains an empty command".into());
    }
    commands.push(words);
    Ok(commands)
}

fn render_pipeline(commands: &[Vec<String>]) -> String {
    commands
        .iter()
        .map(|words| {
            let mut words = words.clone();
            let git_subcommand = words
                .first()
                .is_some_and(|word| word == "git")
                .then(|| words.get(1).cloned())
                .flatten();
            if git_subcommand.is_some() {
                words.insert(1, "-c".into());
                words.insert(2, "core.fsmonitor=false".into());
                words.insert(3, "-c".into());
                words.insert(4, "core.hooksPath=/dev/null".into());
            }
            if git_subcommand
                .as_deref()
                .is_some_and(|word| matches!(word, "diff" | "log" | "show"))
            {
                words.insert(6, "--no-ext-diff".into());
                words.insert(7, "--no-textconv".into());
            }
            let command = words
                .iter()
                .map(|word| format!("'{}'", word.replace('\'', "'\"'\"'")))
                .collect::<Vec<_>>()
                .join(" ");
            if git_subcommand.is_some() {
                format!("GIT_OPTIONAL_LOCKS=0 {command}")
            } else {
                command
            }
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

pub(super) fn inspection_path() -> Option<std::ffi::OsString> {
    let cwd = std::env::current_dir().ok();
    let canonical_cwd = cwd.as_ref().and_then(|path| path.canonicalize().ok());
    let paths = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter(|path| path.is_absolute())
        .filter(|path| {
            let resolved = path.canonicalize().unwrap_or_else(|_| path.clone());
            cwd.as_ref().is_none_or(|cwd| !path.starts_with(cwd))
                && canonical_cwd
                    .as_ref()
                    .is_none_or(|cwd| !resolved.starts_with(cwd))
        })
        .collect::<Vec<_>>();
    std::env::join_paths(paths).ok()
}

fn push_word(words: &mut Vec<String>, word: &mut String) {
    if !word.is_empty() {
        words.push(std::mem::take(word));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn command(command: &str) -> Value {
        json!({"command": command})
    }

    #[test]
    fn accepts_common_repository_inspection_pipelines() {
        for safe in [
            "rg --files | head -n 40",
            "rg -n 'TurnMode|PlanState' src",
            "git status --short",
            "git diff -- src/tools/mod.rs",
            "sed -n '1,120p' README.md",
            "ls -la src",
        ] {
            assert!(prepare(&command(safe)).is_ok(), "rejected {safe}");
        }
    }

    #[test]
    fn rejects_mutation_and_command_escape_routes() {
        for unsafe_command in [
            "printf nope > changed.txt",
            "rm -rf .",
            "rg --pre='touch changed.txt' pattern .",
            "git reset --hard",
            "git diff --output=changed.patch",
            "git diff --ext",
            "find . -delete",
            "cat README.md; touch changed.txt",
            "cat $(touch changed.txt)",
            "cat README.md | sh",
            "sed -i '' 's/a/b/' README.md",
            "sed -n '1p' --in-place README.md",
            "sed '1e touch changed.txt' README.md",
        ] {
            assert!(
                prepare(&command(unsafe_command)).is_err(),
                "accepted {unsafe_command}"
            );
        }
        assert!(
            prepare(&json!({"command": "rg --files", "background": true})).is_err(),
            "planning must reject background execution"
        );
    }

    #[test]
    fn rebuilds_the_pipeline_without_shell_expansion_and_disables_git_hooks() {
        let prepared =
            prepare(&command("rg -n 'Plan State' src | head -n 2")).expect("safe pipeline");
        assert_eq!(
            prepared["command"],
            "'rg' '-n' 'Plan State' 'src' | 'head' '-n' '2'"
        );

        let git = prepare(&command("git diff -- src")).expect("safe git diff");
        assert_eq!(
            git["command"],
            "GIT_OPTIONAL_LOCKS=0 'git' '-c' 'core.fsmonitor=false' '-c' 'core.hooksPath=/dev/null' 'diff' '--no-ext-diff' '--no-textconv' '--' 'src'"
        );
    }
}
