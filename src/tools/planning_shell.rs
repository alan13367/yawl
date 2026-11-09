//! Conservative command gate for read-only planning inspection.

use serde_json::Value;

const ALLOWED_COMMANDS: &[&str] = &[
    "basename", "cat", "cut", "dirname", "du", "find", "git", "grep", "head", "ls", "pwd",
    "readlink", "realpath", "rg", "sed", "stat", "tail", "tr", "wc", "which",
];

/// The redirections that can only discard or merge output, as the rebuilt
/// command writes them.
const DISCARD_STDERR: &str = "2>/dev/null";
const MERGE_STDERR: &str = "2>&1";
const DISCARD_STDOUT: &str = ">/dev/null";
/// Settings that stop Git running programs a repository's config names. Signature
/// checks run them for `--show-signature` and for `%G` log formats alike.
const GIT_PROGRAMS_OFF: &[&str] = &[
    "core.fsmonitor=false",
    "core.hooksPath=/dev/null",
    "gpg.program=false",
    "gpg.ssh.program=false",
    "gpg.x509.program=false",
];
const REDIRECT_REJECTED: &str =
    "shell redirection is unavailable during planning, except 2>/dev/null, 2>&1, and >/dev/null";

/// Planning keeps the full tool list advertised so the prompt prefix stays
/// cacheable across plan phases; this note tells the model what execution
/// accepts while planning.
pub(super) fn restriction_note() -> String {
    format!(
        "Planning is read-only even though every tool stays listed. Only read_file, read_skill, web tools, request_user_input, and the planning tools run; shell accepts only read-only inspection commands, joined by pipes, &&, ||, or ;, from: {}. Git is limited to read-only subcommands, sed to print-only ranges, find cannot run or delete anything, and rg cannot use preprocessors. Only {DISCARD_STDERR}, {MERGE_STDERR}, and {DISCARD_STDOUT} redirect; other redirection, expansion, background execution, and every other tool call are rejected.",
        ALLOWED_COMMANDS.join(", ")
    )
}

/// One parsed command: its words, any harmless redirections, and the operator
/// joining it to the next command.
#[derive(Debug, Default)]
struct PlannedCommand {
    words: Vec<String>,
    redirects: Vec<&'static str>,
    then: Option<&'static str>,
}

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
    for planned in &commands {
        validate_command(&planned.words)?;
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
        // Prefixes, so these also cover -execdir, -okdir, and -fprintf.
        "find" => reject_options(words, &["-exec", "-ok", "-delete", "-fprint", "-fls"]),
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
    // `git grep -O<pager>` runs a command, and short options bundle, so
    // `-nO<pager>` means the same thing. Refusing a pattern glued to `-e` that
    // contains an O is a small price.
    if subcommand == "grep"
        && let Some(option) = words
            .iter()
            .skip(2)
            .find(|word| word.starts_with('-') && !word.starts_with("--") && word.contains('O'))
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

/// Consumes the rest of a redirection after its `>`. The target must be one
/// that cannot change anything, so it is matched here instead of being passed
/// on to the shell.
fn parse_redirect(
    source_fd: &str,
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Result<&'static str, String> {
    while matches!(chars.peek(), Some(' ' | '\t')) {
        chars.next();
    }
    let mut target = String::new();
    while let Some(ch) = chars.peek().copied() {
        if matches!(ch, ' ' | '\t' | '|' | ';') || (ch == '&' && !target.is_empty()) {
            break;
        }
        target.push(ch);
        chars.next();
    }
    match (source_fd, target.as_str()) {
        ("2", "/dev/null") => Ok(DISCARD_STDERR),
        ("2", "&1") => Ok(MERGE_STDERR),
        ("" | "1", "/dev/null") => Ok(DISCARD_STDOUT),
        _ => Err(REDIRECT_REJECTED.into()),
    }
}

fn parse_pipeline(command: &str) -> Result<Vec<PlannedCommand>, String> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    /// Ends the current command, joined to the next one by `then`.
    fn finish(
        commands: &mut Vec<PlannedCommand>,
        current: &mut PlannedCommand,
        words: &mut Vec<String>,
        then: &'static str,
    ) -> Result<(), String> {
        if words.is_empty() {
            return Err("planning shell pipeline contains an empty command".into());
        }
        current.words = std::mem::take(words);
        current.then = Some(then);
        commands.push(std::mem::take(current));
        Ok(())
    }

    let mut commands = Vec::new();
    let mut current = PlannedCommand::default();
    let mut words = Vec::new();
    let mut word = String::new();
    // A quoted word counts even when empty, as in `rg '' src`.
    let mut quoted = false;
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
                // Inside double quotes a backslash only escapes these; before
                // anything else it stays, as in the pattern "\bword\b".
                '\\' => match chars.next() {
                    Some(next @ ('"' | '\\' | '$' | '`')) => word.push(next),
                    Some('\n' | '\r') => {
                        return Err(
                            "multiline shell commands are unavailable during planning".into()
                        );
                    }
                    Some(next) => {
                        word.push('\\');
                        word.push(next);
                    }
                    None => return Err("planning shell command ends with an escape".into()),
                },
                '\n' | '\r' => {
                    return Err("multiline shell commands are unavailable during planning".into());
                }
                _ => word.push(ch),
            },
            Quote::None => match ch {
                '\'' => {
                    quote = Quote::Single;
                    quoted = true;
                }
                '"' => {
                    quote = Quote::Double;
                    quoted = true;
                }
                '\\' => {
                    word.push(
                        chars.next().ok_or_else(|| {
                            "planning shell command ends with an escape".to_string()
                        })?,
                    );
                    quoted = true;
                }
                ' ' | '\t' => push_word(&mut words, &mut word, &mut quoted),
                '|' => {
                    push_word(&mut words, &mut word, &mut quoted);
                    let then = if chars.peek() == Some(&'|') {
                        chars.next();
                        "||"
                    } else {
                        "|"
                    };
                    finish(&mut commands, &mut current, &mut words, then)?;
                }
                ';' => {
                    push_word(&mut words, &mut word, &mut quoted);
                    finish(&mut commands, &mut current, &mut words, ";")?;
                }
                '&' => {
                    if chars.peek() != Some(&'&') {
                        return Err("background commands are unavailable during planning".into());
                    }
                    chars.next();
                    push_word(&mut words, &mut word, &mut quoted);
                    finish(&mut commands, &mut current, &mut words, "&&")?;
                }
                '<' => return Err(REDIRECT_REJECTED.into()),
                '>' => {
                    // Only bare digits touching the `>` name a stream; any
                    // other word before it is an ordinary argument.
                    let names_stream =
                        !quoted && !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit());
                    let stream = if names_stream {
                        std::mem::take(&mut word)
                    } else {
                        push_word(&mut words, &mut word, &mut quoted);
                        String::new()
                    };
                    current.redirects.push(parse_redirect(&stream, &mut chars)?);
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
    push_word(&mut words, &mut word, &mut quoted);
    if words.is_empty() {
        // Only `;` may end a command line; `cat a |` still waits for a command.
        return match commands.last_mut() {
            Some(last) if last.then == Some(";") => {
                last.then = None;
                Ok(commands)
            }
            _ => Err("planning shell pipeline contains an empty command".into()),
        };
    }
    current.words = words;
    commands.push(current);
    Ok(commands)
}

fn render_pipeline(commands: &[PlannedCommand]) -> String {
    let mut rendered = String::new();
    for planned in commands {
        rendered.push_str(&render_command(&planned.words));
        for redirect in &planned.redirects {
            rendered.push(' ');
            rendered.push_str(redirect);
        }
        if let Some(then) = planned.then {
            if then != ";" {
                rendered.push(' ');
            }
            rendered.push_str(then);
            rendered.push(' ');
        }
    }
    rendered
}

fn render_command(words: &[String]) -> String {
    let quote = |words: &[String]| {
        words
            .iter()
            .map(|word| format!("'{}'", word.replace('\'', "'\"'\"'")))
            .collect::<Vec<_>>()
            .join(" ")
    };
    match words {
        [git, subcommand, rest @ ..] if git == "git" => {
            let mut rebuilt = vec![git.clone()];
            // A repository's own config can name programs for Git to run.
            // Planning must not run them, so each is turned off by name.
            for setting in GIT_PROGRAMS_OFF
                .iter()
                .map(|setting| (*setting).to_string())
                .chain(super::git::repository_filters_off(std::path::Path::new(
                    ".",
                )))
            {
                rebuilt.push("-c".into());
                rebuilt.push(setting);
            }
            rebuilt.push(subcommand.clone());
            let flags: &[&str] = match subcommand.as_str() {
                "diff" | "log" | "show" => &["--no-ext-diff", "--no-textconv"],
                "blame" => &["--no-textconv"],
                _ => &[],
            };
            rebuilt.extend(flags.iter().map(|flag| (*flag).to_string()));
            rebuilt.extend_from_slice(rest);
            format!("GIT_OPTIONAL_LOCKS=0 {}", quote(&rebuilt))
        }
        _ => quote(words),
    }
}

pub(super) fn inspection_path() -> Option<std::ffi::OsString> {
    let cwd = std::env::current_dir().ok();
    let canonical_cwd = cwd
        .as_ref()
        .and_then(|path| crate::paths::canonicalize(path).ok());
    let paths = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter(|path| path.is_absolute())
        .filter(|path| {
            let resolved = crate::paths::canonicalize(path).unwrap_or_else(|_| path.clone());
            cwd.as_ref().is_none_or(|cwd| !path.starts_with(cwd))
                && canonical_cwd
                    .as_ref()
                    .is_none_or(|cwd| !resolved.starts_with(cwd))
        })
        .collect::<Vec<_>>();
    std::env::join_paths(paths).ok()
}

fn push_word(words: &mut Vec<String>, word: &mut String, quoted: &mut bool) {
    if !word.is_empty() || *quoted {
        words.push(std::mem::take(word));
    }
    *quoted = false;
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
            "find src -name '*.rs' -type f",
            "which rg",
            "rg foo src 2>/dev/null",
            "rg foo src 2> /dev/null | head -n 5",
            "git status --short 2>&1",
            "ls missing >/dev/null",
            "ls src && git status --short",
            "cat a.txt; cat b.txt;",
            "ls missing 2>/dev/null || ls src",
        ] {
            assert!(prepare(&command(safe)).is_ok(), "rejected {safe}");
        }
    }

    #[test]
    fn git_grep_cannot_name_a_pager_inside_a_bundle_of_options() {
        for escape in [
            "git grep -Otouch foo",
            "git grep -nOtouch foo",
            "git grep -hOvim foo",
            "git grep -inO foo",
            "git grep --open-files-in-pager=touch foo",
            "git grep --open foo",
        ] {
            assert!(prepare(&command(escape)).is_err(), "accepted {escape}");
        }
        for fine in [
            "git grep -n foo",
            "git grep -n Order",
            "git grep -n -e foo",
            "git log -SOrder",
            "git log --oneline -n 5",
        ] {
            assert!(prepare(&command(fine)).is_ok(), "rejected {fine}");
        }
    }

    #[test]
    fn programs_a_repository_config_names_are_turned_off() {
        for (given, expected) in [
            ("git blame src/a.rs", "'blame' '--no-textconv' 'src/a.rs'"),
            ("git log --show-signature", "'gpg.program=false'"),
            ("git status", "'core.fsmonitor=false'"),
        ] {
            let prepared = prepare(&command(given)).expect("safe git command");
            let rendered = prepared["command"].as_str().expect("command");
            assert!(rendered.contains(expected), "{rendered}");
        }
    }

    #[test]
    fn quoting_reaches_the_program_the_way_a_shell_would_pass_it() {
        for (given, expected) in [
            // A backslash in double quotes only escapes a few characters.
            (r#"rg "\bfoo\b" src"#, r"'rg' '\bfoo\b' 'src'"),
            (r#"grep "a\.b" f"#, r"'grep' 'a\.b' 'f'"),
            (r#"rg "say \"hi\"" src"#, r#"'rg' 'say "hi"' 'src'"#),
            (r#"rg "a\\b" src"#, r"'rg' 'a\b' 'src'"),
            (r#"rg "cost \$5" src"#, "'rg' 'cost $5' 'src'"),
            // An empty argument is still an argument.
            ("rg '' src", "'rg' '' 'src'"),
            (r#"rg "" src"#, "'rg' '' 'src'"),
            // Only bare digits touching `>` name a stream.
            (r#"ls "2">/dev/null"#, "'ls' '2' >/dev/null"),
            (r"ls \2>/dev/null", "'ls' '2' >/dev/null"),
            ("ls 2>/dev/null", "'ls' 2>/dev/null"),
            ("ls 2 >/dev/null", "'ls' '2' >/dev/null"),
            ("ls src>/dev/null", "'ls' 'src' >/dev/null"),
        ] {
            let prepared = prepare(&command(given)).expect(given);
            assert_eq!(prepared["command"], expected, "{given}");
        }
        // An empty command name is still not an allowed command.
        assert!(prepare(&command("'' ls")).is_err());
        assert!(prepare(&command("rg \"a\\\nb\" src")).is_err(), "multiline");
    }

    #[test]
    fn every_command_in_a_chain_is_checked() {
        for unsafe_command in [
            "ls && rm -rf .",
            "ls; rm -rf .",
            "ls || git reset --hard",
            "ls && find . -delete",
            "find . -exec touch {} +",
            "find . -execdir rm {} +",
            "find . -okdir rm {} +",
            "find . -fprint out.txt",
            "find . -fprintf out.txt %p",
            "find . -fls out.txt",
        ] {
            assert!(
                prepare(&command(unsafe_command)).is_err(),
                "accepted {unsafe_command}"
            );
        }
    }

    #[test]
    fn only_harmless_redirection_and_chaining_forms_pass() {
        for rejected in [
            "cat a > b",
            "cat a >> b",
            "cat a 2> errors.log",
            "cat a 2>&2",
            "cat a >&2",
            "cat a 3>/dev/null",
            "cat a >/dev/null2",
            "cat a &> out",
            "cat a < b",
            "cat a &",
            "cat a & cat b",
            "&& cat a",
            "cat a &&",
            "cat a ||",
            "cat a |",
            "; cat a",
            "cat a ;; cat b",
        ] {
            assert!(prepare(&command(rejected)).is_err(), "accepted {rejected}");
        }
    }

    #[test]
    fn chains_and_redirections_are_rebuilt_literally() {
        assert_eq!(
            prepare(&command("rg foo 2>/dev/null && ls;")).expect("safe chain")["command"],
            "'rg' 'foo' 2>/dev/null && 'ls'"
        );
        assert_eq!(
            prepare(&command("ls a; ls b || ls c | wc -l")).expect("safe chain")["command"],
            "'ls' 'a'; 'ls' 'b' || 'ls' 'c' | 'wc' '-l'"
        );
        // A quoted operator is only an argument.
        assert_eq!(
            prepare(&command("rg 'a && b > c'")).expect("quoted")["command"],
            "'rg' 'a && b > c'"
        );
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
            "GIT_OPTIONAL_LOCKS=0 'git' '-c' 'core.fsmonitor=false' '-c' 'core.hooksPath=/dev/null' '-c' 'gpg.program=false' '-c' 'gpg.ssh.program=false' '-c' 'gpg.x509.program=false' 'diff' '--no-ext-diff' '--no-textconv' '--' 'src'"
        );
    }
}
