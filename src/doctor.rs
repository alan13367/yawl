//! Configuration doctor. Diagnoses the global and project config files and
//! the saved login file, reports findings, and applies repairs the user
//! confirms. Works even when `Config::load` fails, because that is exactly
//! when it is needed.

mod checks;
mod repair;
mod report;

use std::io::IsTerminal;
use std::path::PathBuf;

use crate::error::Error;

/// Files the doctor examines.
pub(crate) struct Paths {
    pub(crate) global: PathBuf,
    pub(crate) project: PathBuf,
    pub(crate) auth: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Debug)]
pub(crate) struct Finding {
    pub(crate) severity: Severity,
    /// Short label naming the file or section, e.g. `global config`.
    pub(crate) area: String,
    pub(crate) message: String,
    pub(crate) fix: Option<Fix>,
}

#[derive(Clone, Debug)]
pub(crate) enum Fix {
    /// Renames an unparseable file aside so defaults regenerate.
    QuarantineFile { path: PathBuf },
    /// Removes a nested key; array indexes are digits.
    RemoveKey { path: PathBuf, keys: Vec<String> },
    /// Sets a nested key to a replacement value.
    SetValue {
        path: PathBuf,
        keys: Vec<String>,
        value: serde_json::Value,
    },
    /// Restricts file permissions to owner-only.
    ChmodPrivate { path: PathBuf },
    /// Moves a backup over the live file.
    RestoreBackup { from: PathBuf, to: PathBuf },
}

impl Fix {
    /// One line describing what applying this fix does.
    pub(crate) fn describe(&self) -> String {
        match self {
            Fix::QuarantineFile { path } => {
                format!("rename {} aside so defaults regenerate", path.display())
            }
            Fix::RemoveKey { keys, .. } => format!("remove {}", dotted(keys)),
            Fix::SetValue { keys, value, .. } => {
                format!("set {} to {}", dotted(keys), value)
            }
            Fix::ChmodPrivate { path } => format!("chmod 600 {}", path.display()),
            Fix::RestoreBackup { from, to } => {
                format!("restore {} over {}", from.display(), to.display())
            }
        }
    }
}

fn dotted(keys: &[String]) -> String {
    keys.join(".")
}

/// Returns the value at a nested key path. Digit segments index arrays.
pub(crate) fn value_at<'a>(
    value: &'a serde_json::Value,
    keys: &[String],
) -> Option<&'a serde_json::Value> {
    let mut current = value;
    for key in keys {
        current = match current {
            serde_json::Value::Object(map) => map.get(key.as_str()),
            serde_json::Value::Array(items) => {
                key.parse::<usize>().ok().and_then(|index| items.get(index))
            }
            _ => None,
        }?;
    }
    Some(current)
}

/// Removes the value at a nested key path, creating nothing. Returns false
/// when the path does not exist.
pub(crate) fn remove_nested(value: &mut serde_json::Value, keys: &[String]) -> bool {
    let Some((first, rest)) = keys.split_first() else {
        return false;
    };
    if rest.is_empty() {
        return match value {
            serde_json::Value::Object(map) => map.remove(first.as_str()).is_some(),
            serde_json::Value::Array(items) => first
                .parse::<usize>()
                .ok()
                .and_then(|index| (index < items.len()).then(|| items.remove(index)))
                .is_some(),
            _ => false,
        };
    }
    let child = match value {
        serde_json::Value::Object(map) => map.get_mut(first.as_str()),
        serde_json::Value::Array(items) => first
            .parse::<usize>()
            .ok()
            .and_then(|index| items.get_mut(index)),
        _ => None,
    };
    match child {
        Some(child) => remove_nested(child, rest),
        None => false,
    }
}

/// Inserts `replacement` at a nested key path, creating intermediate
/// objects. Array segments must already exist.
pub(crate) fn set_nested(
    value: &mut serde_json::Value,
    keys: &[String],
    replacement: serde_json::Value,
) -> bool {
    let Some((first, rest)) = keys.split_first() else {
        return false;
    };
    if rest.is_empty() {
        return match value {
            serde_json::Value::Object(map) => {
                map.insert(first.clone(), replacement);
                true
            }
            serde_json::Value::Array(items) => match first.parse::<usize>() {
                Ok(index) if index < items.len() => {
                    items[index] = replacement;
                    true
                }
                _ => false,
            },
            _ => false,
        };
    }
    let serde_json::Value::Object(map) = value else {
        return false;
    };
    let child = map
        .entry(first.clone())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    set_nested(child, rest, replacement)
}

/// Runs the doctor. Returns the process exit code: 0 when no errors remain,
/// 1 otherwise, so scripts can rely on it.
///
/// # Errors
///
/// Returns an error when a repair fails on disk input or output.
pub fn run() -> Result<i32, Error> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error::Config("HOME is not set".into()))?;
    let paths = Paths {
        global: home.join(".yawl/config.json"),
        project: PathBuf::from(".yawl/config.json"),
        auth: home.join(".yawl/auth.json"),
    };

    let findings = checks::run(&paths);
    report::print(&findings);
    if std::io::stdin().is_terminal() && repair::offer(&findings)? {
        let findings = checks::run(&paths);
        report::print(&findings);
        return Ok(report::exit_code(&findings));
    }
    Ok(report::exit_code(&findings))
}
