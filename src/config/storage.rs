use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use serde_json::{Map, Value};

use super::Config;
use crate::error::Error;

impl Config {
    pub(super) fn update_global_json(
        &self,
        update: impl FnOnce(&mut Map<String, Value>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let path = self.global_config_path();
        let mut root = read_json_object(&path)?;
        update(&mut root)?;
        write_json_object(&path, &root)
    }
}

pub(super) fn validate_provider_name(name: &str) -> Result<(), Error> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(Error::Config(
            "provider name may contain only letters, numbers, '-' and '_'".into(),
        ));
    }
    Ok(())
}

pub(super) fn object_field<'a>(
    root: &'a mut Map<String, Value>,
    name: &str,
) -> Result<&'a mut Map<String, Value>, Error> {
    let value = root
        .entry(name.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(Error::Config(format!("'{name}' must be a JSON object"))),
    }
}

pub(crate) fn read_json_object(path: &Path) -> Result<Map<String, Value>, Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str(&text)
            .map_err(|error| Error::Config(format!("{}: {error}", path.display())))?
        {
            Value::Object(object) => Ok(object),
            _ => Err(Error::Config(format!(
                "{}: top-level JSON value must be an object",
                path.display()
            ))),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(Error::Config(format!("{}: {error}", path.display()))),
    }
}

pub(crate) fn write_json_object(path: &Path, root: &Map<String, Value>) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        serde_json::to_writer_pretty(&mut file, &Value::Object(root.clone()))?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok::<(), Error>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// One piece of a provider key or header value.
pub(crate) enum ValuePart {
    Text(String),
    /// Name of an environment variable referenced as `$NAME` or `${NAME}`.
    Variable(String),
}

/// Why a provider key or header value cannot be parsed.
pub(crate) enum ValueError {
    /// A leading `!`, which pi uses for shell commands.
    ShellCommand,
    /// A `${` without its closing brace.
    Unterminated,
}

/// Splits a provider key or header value into text and pi-style
/// environment references. `$$` and `$!` are a literal `$` and `!`, and a
/// `$` that starts no name stays literal.
pub(crate) fn config_value_parts(value: &str) -> Result<Vec<ValuePart>, ValueError> {
    if value.starts_with('!') {
        return Err(ValueError::ShellCommand);
    }
    let chars = value.chars().collect::<Vec<_>>();
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] != '$' {
            text.push(chars[index]);
            index += 1;
            continue;
        }
        let Some(next) = chars.get(index + 1).copied() else {
            text.push('$');
            break;
        };
        if matches!(next, '$' | '!') {
            text.push(next);
            index += 2;
            continue;
        }
        let (name, next_index) = if next == '{' {
            let Some(relative_end) = chars[index + 2..]
                .iter()
                .position(|character| *character == '}')
            else {
                return Err(ValueError::Unterminated);
            };
            let end = index + 2 + relative_end;
            (chars[index + 2..end].iter().collect::<String>(), end + 1)
        } else {
            let end = chars[index + 1..]
                .iter()
                .position(|character| !(character.is_ascii_alphanumeric() || *character == '_'))
                .map_or(chars.len(), |relative| index + 1 + relative);
            if end == index + 1 {
                text.push('$');
                index += 1;
                continue;
            }
            (chars[index + 1..end].iter().collect::<String>(), end)
        };
        if !text.is_empty() {
            parts.push(ValuePart::Text(std::mem::take(&mut text)));
        }
        parts.push(ValuePart::Variable(name));
        index = next_index;
    }
    if !text.is_empty() {
        parts.push(ValuePart::Text(text));
    }
    Ok(parts)
}

/// Resolves pi-style `$ENV_VAR` and `${ENV_VAR}` references in provider
/// keys and header values. `$$` emits `$` and `$!` emits `!`.
pub(crate) fn resolve_config_value(value: &str) -> Result<String, Error> {
    let parts = config_value_parts(value).map_err(|error| {
        Error::Config(
            match error {
                ValueError::ShellCommand => {
                    "provider values beginning with '!' are not supported; use an environment variable"
                }
                ValueError::Unterminated => "unterminated environment variable reference",
            }
            .into(),
        )
    })?;
    let mut output = String::with_capacity(value.len());
    for part in parts {
        match part {
            ValuePart::Text(text) => output.push_str(&text),
            ValuePart::Variable(name) => {
                output.push_str(&std::env::var(&name).map_err(|_| {
                    Error::Config(format!("environment variable {name} is not set"))
                })?)
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_value_escapes_literal_markers() -> Result<(), Error> {
        assert_eq!(resolve_config_value("$$money-$!bang")?, "$money-!bang");
        Ok(())
    }
}
