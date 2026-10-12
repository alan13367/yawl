//! Custom tools: JSON manifests that personalize Yawl for one user or project.
//!
//! A custom tool is `<name>.json` in `~/.yawl/tools/` (global) or
//! `./.yawl/tools/` (project). The manifest declares typed parameters and a
//! `sh -c` command whose `{{param}}` placeholders expand to shell-quoted
//! values; the complete JSON arguments also arrive on stdin. Discovery only
//! parses JSON, so nothing runs until the model calls a tool. Project tools
//! load once the project is trusted, except manifests this process saved.
//!
//! The model creates and updates tools through `define_tool`, which runs the
//! same validation as discovery and reports problems before anything is saved.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::exec::valid_tool_name;
use super::{ToolEntry, ToolImpl, ToolOutcome};
use crate::config::Config;
use crate::provider::ToolSpec;

pub(crate) const DEFINE_TOOL_NAME: &str = "define_tool";
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 24 * 60 * 60;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_DESCRIPTION_CHARS: usize = 1_000;
const MAX_PARAMS: usize = 32;
/// How much of an executable is searched for the old `--describe` protocol.
const MAX_LEGACY_SCAN_BYTES: u64 = 8 * 1024 * 1024;
const DEFINE_FIELDS: &[&str] = &[
    "name",
    "description",
    "command",
    "params",
    "scope",
    "timeout_secs",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Scope {
    Global,
    Project,
}

impl Scope {
    pub(super) fn label(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Project => "project",
        }
    }
}

/// On-disk manifest. Unknown fields are rejected so a misspelled key fails
/// loudly instead of silently dropping a parameter.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    description: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    params: BTreeMap<String, Param>,
    command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Param {
    #[serde(rename = "type", default)]
    kind: ParamType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    required: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default: Option<Value>,
    #[serde(rename = "enum", default, skip_serializing_if = "Option::is_none")]
    choices: Option<Vec<Value>>,
    /// Booleans only: text inserted for `true`; `false` inserts nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    flag: Option<String>,
}

impl Param {
    /// Parameters are required unless they opt out, carry a default, or
    /// are flags, whose absence simply means "off".
    fn is_required(&self) -> bool {
        self.required
            .unwrap_or(self.default.is_none() && self.flag.is_none())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ParamType {
    #[default]
    String,
    Integer,
    Number,
    Boolean,
    Array,
    Object,
}

impl ParamType {
    fn as_str(self) -> &'static str {
        match self {
            ParamType::String => "string",
            ParamType::Integer => "integer",
            ParamType::Number => "number",
            ParamType::Boolean => "boolean",
            ParamType::Array => "array",
            ParamType::Object => "object",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            ParamType::String => "a string",
            ParamType::Integer => "an integer",
            ParamType::Number => "a number",
            ParamType::Boolean => "a boolean",
            ParamType::Array => "an array of strings",
            ParamType::Object => "a JSON object",
        }
    }

    /// Exact type check for manifest defaults and enum values.
    fn matches(self, value: &Value) -> bool {
        match self {
            ParamType::String => value.is_string(),
            ParamType::Integer => value.is_i64() || value.is_u64(),
            ParamType::Number => value.is_number(),
            ParamType::Boolean => value.is_boolean(),
            ParamType::Array => value
                .as_array()
                .is_some_and(|items| items.iter().all(is_scalar)),
            ParamType::Object => value.is_object(),
        }
    }

    /// Lenient conversion for call arguments. Models often send `"3"` for
    /// an integer or a lone string for a list; accept what is unambiguous.
    fn coerce(self, value: &Value) -> Option<Value> {
        match (self, value) {
            (ParamType::String, Value::String(_)) => Some(value.clone()),
            (ParamType::String, Value::Number(_) | Value::Bool(_)) => {
                Some(Value::String(value.to_string()))
            }
            (ParamType::Integer, Value::Number(number)) => {
                if number.is_f64() {
                    number.as_f64().and_then(whole_number)
                } else {
                    Some(value.clone())
                }
            }
            (ParamType::Integer, Value::String(text)) => {
                text.trim().parse::<i64>().ok().map(Value::from)
            }
            (ParamType::Number, Value::Number(number)) => match number.as_f64() {
                Some(float) if number.is_f64() => number_value(float),
                _ => Some(value.clone()),
            },
            (ParamType::Number, Value::String(text)) => {
                text.trim().parse::<f64>().ok().and_then(number_value)
            }
            (ParamType::Boolean, Value::Bool(_)) => Some(value.clone()),
            (ParamType::Boolean, Value::String(text)) => match text.trim() {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                _ => None,
            },
            (ParamType::Array, Value::Array(items)) => items
                .iter()
                .map(|item| is_scalar(item).then(|| Value::String(scalar_text(item))))
                .collect::<Option<Vec<_>>>()
                .map(Value::Array),
            (ParamType::Array, Value::String(_)) => Some(Value::Array(vec![value.clone()])),
            (ParamType::Object, Value::Object(_)) => Some(value.clone()),
            (ParamType::Object, Value::String(text)) => serde_json::from_str::<Value>(text)
                .ok()
                .filter(Value::is_object),
            _ => None,
        }
    }
}

/// An integral float as an integer, or `None` when it has a fraction or
/// cannot be represented exactly.
fn whole_number(float: f64) -> Option<Value> {
    (float.fract() == 0.0 && float.abs() < 9.0e15).then(|| json!(float as i64))
}

/// Numbers keep integers integral, so `"1"` and `1.0` match an `enum` of
/// `[1, 2]` and expand to `1` rather than `1.0`.
fn number_value(float: f64) -> Option<Value> {
    whole_number(float).or_else(|| serde_json::Number::from_f64(float).map(Value::Number))
}

fn is_scalar(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_))
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// A loaded manifest, ready to run.
pub(super) struct CustomTool {
    pub(super) scope: Scope,
    pub(super) path: PathBuf,
    params: BTreeMap<String, Param>,
    command: String,
    timeout: Duration,
}

impl CustomTool {
    pub(super) fn run(&self, name: &str, args: &Value, session_id: &str) -> ToolOutcome {
        let values = match self.normalize(args) {
            Ok(values) => values,
            Err(error) => {
                return ToolOutcome::error(format!("{error}. Usage: {}", self.signature(name)));
            }
        };
        let command = self.expand(&values);
        let input = Value::Object(values).to_string();
        let mut process = Command::new("sh");
        process
            .arg("-c")
            .arg(&command)
            .env("YAWL_SESSION_ID", session_id);
        // Absolute, so commands that change directory can still reach
        // helper scripts next to the manifest.
        if let Some(directory) = self.path.parent() {
            let directory =
                std::fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
            process.env("YAWL_TOOL_DIR", directory);
        }
        match super::exec::run_with_timeout(process, Some(input.as_bytes()), self.timeout) {
            Ok(result) => {
                let (content, is_error) = super::exec::render_result(&result, self.timeout);
                ToolOutcome {
                    content,
                    images: Vec::new(),
                    is_error,
                }
            }
            Err(error) => ToolOutcome::error(format!("failed to start {name}: {error}")),
        }
    }

    /// `name(required: type, optional?: type)`, the compact form used in
    /// argument errors and `define_tool` confirmations.
    fn signature(&self, name: &str) -> String {
        signature(name, &self.params)
    }

    /// The fixed text the command starts with, up to its first placeholder,
    /// when it is specific enough to recognize: a program and at least one
    /// argument on one line. `git {{args}}` is too generic to match. The flag
    /// says whether placeholders follow and so may absorb more arguments.
    fn literal_head(&self) -> Option<(&str, bool)> {
        let segments = segments(&self.command);
        let head = match segments.first() {
            Some((_, Segment::Text(text))) => text.trim(),
            _ => return None,
        };
        (head.contains(' ') && !head.contains('\n')).then_some((head, segments.len() > 1))
    }

    /// Validates call arguments, applies defaults, and coerces values to the
    /// declared types. The result is both the stdin payload and the source
    /// of placeholder values.
    fn normalize(&self, args: &Value) -> Result<Map<String, Value>, String> {
        let empty = Map::new();
        let object = match args {
            Value::Object(object) => object,
            Value::Null => &empty,
            _ => return Err("arguments must be a JSON object".into()),
        };
        if let Some(unknown) = object.keys().find(|key| !self.params.contains_key(*key)) {
            return Err(if self.params.is_empty() {
                format!("unknown argument '{unknown}'; this tool takes no arguments")
            } else {
                format!(
                    "unknown argument '{unknown}'; parameters are {}",
                    self.params.keys().cloned().collect::<Vec<_>>().join(", ")
                )
            });
        }
        let mut values = Map::new();
        for (name, param) in &self.params {
            let value = match object.get(name) {
                None | Some(Value::Null) => param.default.clone(),
                Some(value) => Some(param.kind.coerce(value).ok_or_else(|| {
                    format!("argument '{name}' must be {}", param.kind.describe())
                })?),
            };
            let Some(value) = value else {
                if param.is_required() {
                    return Err(format!("missing required argument '{name}'"));
                }
                continue;
            };
            if let Some(choices) = &param.choices
                && !choices.contains(&value)
            {
                return Err(format!(
                    "argument '{name}' must be one of {}",
                    choices
                        .iter()
                        .map(Value::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            values.insert(name.clone(), value);
        }
        Ok(values)
    }

    fn expand(&self, values: &Map<String, Value>) -> String {
        let mut command = String::with_capacity(self.command.len());
        for (_, segment) in segments(&self.command) {
            match segment {
                Segment::Text(text) => command.push_str(text),
                Segment::Param(name) => {
                    if let (Some(param), Some(value)) = (self.params.get(name), values.get(name)) {
                        command.push_str(&render_value(param, value));
                    }
                }
            }
        }
        command
    }
}

/// A note for a shell command that a custom tool already wraps. Models keep
/// typing familiar commands after saving a tool for them; pointing at the
/// tool each time is what turns a saved tool into a habit.
pub(super) fn shell_hint(entries: &[Arc<ToolEntry>], command: &str) -> Option<String> {
    let command = command.trim_start();
    entries.iter().find_map(|entry| {
        let ToolImpl::Custom(tool) = &entry.imp else {
            return None;
        };
        let (head, takes_arguments) = tool.literal_head()?;
        let rest = command.strip_prefix(head)?;
        // A tool without placeholders cannot take extra arguments, so only
        // the exact command is a match.
        let matches = if takes_arguments {
            rest.chars().next().is_none_or(char::is_whitespace)
        } else {
            rest.trim().is_empty()
        };
        matches.then(|| {
            format!(
                "[yawl] The custom tool `{}` wraps this command; call it instead next time.",
                entry.spec.name
            )
        })
    })
}

fn render_value(param: &Param, value: &Value) -> String {
    match value {
        // Flags are part of the author's command, so they insert as written
        // and may hold several words such as `--features full`.
        Value::Bool(true) if param.flag.is_some() => param.flag.clone().unwrap_or_default(),
        Value::Bool(false) if param.flag.is_some() => String::new(),
        Value::Bool(_) | Value::Number(_) => value.to_string(),
        Value::String(text) => shell_word(text),
        Value::Array(items) => items
            .iter()
            .map(|item| shell_word(&scalar_text(item)))
            .collect::<Vec<_>>()
            .join(" "),
        Value::Object(_) => shell_word(&value.to_string()),
        Value::Null => String::new(),
    }
}

/// Quotes one shell word. Plain words stay readable; everything else is
/// single-quoted with embedded quotes escaped.
fn shell_word(text: &str) -> String {
    if !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_./:,@%+".contains(&byte))
    {
        return text.to_string();
    }
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for character in text.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

fn signature(name: &str, params: &BTreeMap<String, Param>) -> String {
    let params = params
        .iter()
        .map(|(param_name, param)| {
            let optional = if param.is_required() { "" } else { "?" };
            format!("{param_name}{optional}: {}", param.kind.as_str())
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("{name}({params})")
}

enum Segment<'a> {
    Text(&'a str),
    Param(&'a str),
}

/// Splits a command at `{{name}}` placeholders, keeping byte offsets.
/// Braces around anything other than a parameter-style identifier stay
/// literal, so Go templates such as `{{.State}}` pass through untouched.
fn segments(command: &str) -> Vec<(usize, Segment<'_>)> {
    let mut segments = Vec::new();
    let mut text_start = 0;
    let mut search = 0;
    while let Some(found) = command[search..].find("{{") {
        let open = search + found;
        let inner_start = open + 2;
        if let Some(close) = command[inner_start..].find("}}") {
            let name = command[inner_start..inner_start + close].trim();
            if valid_param_name(name) {
                if text_start < open {
                    segments.push((text_start, Segment::Text(&command[text_start..open])));
                }
                segments.push((open, Segment::Param(name)));
                text_start = inner_start + close + 2;
                search = text_start;
                continue;
            }
        }
        search = open + 1;
    }
    if text_start < command.len() {
        segments.push((text_start, Segment::Text(&command[text_start..])));
    }
    segments
}

/// Marks each byte that sits inside single or double quotes.
fn quoted_bytes(command: &str) -> Vec<bool> {
    let bytes = command.as_bytes();
    let mut quoted = vec![false; bytes.len()];
    let mut quote = None;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        quoted[index] = quote.is_some();
        match (quote, byte) {
            (None | Some(b'"'), b'\\') => {
                if let Some(next) = quoted.get_mut(index + 1) {
                    *next = quote.is_some();
                }
                index += 2;
                continue;
            }
            (None, b'\'' | b'"') => quote = Some(byte),
            (Some(open), _) if byte == open => quote = None,
            _ => {}
        }
        index += 1;
    }
    quoted
}

fn valid_param_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= 64
        && bytes
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn validate(manifest: &Manifest) -> Result<(), String> {
    let description = manifest.description.trim();
    if description.is_empty() {
        return Err("description must say what the tool does and when to use it".into());
    }
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(format!(
            "description must be at most {MAX_DESCRIPTION_CHARS} characters"
        ));
    }
    if manifest.command.trim().is_empty() {
        return Err("command must not be empty".into());
    }
    if let Some(timeout) = manifest.timeout_secs
        && !(1..=MAX_TIMEOUT_SECS).contains(&timeout)
    {
        return Err(format!(
            "timeout_secs must be between 1 and {MAX_TIMEOUT_SECS}"
        ));
    }
    if manifest.params.len() > MAX_PARAMS {
        return Err(format!("a tool may declare at most {MAX_PARAMS} params"));
    }
    for (name, param) in &manifest.params {
        validate_param(name, param).map_err(|error| format!("param '{name}': {error}"))?;
    }
    let quoted = quoted_bytes(&manifest.command);
    for (offset, segment) in segments(&manifest.command) {
        let Segment::Param(name) = segment else {
            continue;
        };
        if !manifest.params.contains_key(name) {
            return Err(format!(
                "command uses {{{{{name}}}}} but params does not declare '{name}'"
            ));
        }
        if quoted[offset] {
            return Err(format!(
                "{{{{{name}}}}} is inside quotes; Yawl shell-quotes placeholder values itself, \
                 so remove the surrounding quotes"
            ));
        }
    }
    Ok(())
}

fn validate_param(name: &str, param: &Param) -> Result<(), String> {
    if !valid_param_name(name) {
        return Err(
            "names must start with a letter or '_' and contain only letters, digits, or '_'".into(),
        );
    }
    if let Some(flag) = &param.flag {
        if param.kind != ParamType::Boolean {
            return Err("flag is only valid for boolean params".into());
        }
        if flag.trim().is_empty() || flag.contains('\n') {
            return Err("flag must be one line of command text, such as \"--release\"".into());
        }
    }
    if let Some(choices) = &param.choices {
        if !matches!(
            param.kind,
            ParamType::String | ParamType::Integer | ParamType::Number
        ) {
            return Err("enum is only valid for string, integer, or number params".into());
        }
        if choices.is_empty() {
            return Err("enum must list at least one value".into());
        }
        if let Some(bad) = choices.iter().find(|choice| !param.kind.matches(choice)) {
            return Err(format!("enum value {bad} is not {}", param.kind.describe()));
        }
    }
    if let Some(default) = &param.default {
        if param.required == Some(true) {
            return Err("a param with a default cannot also be required".into());
        }
        if !param.kind.matches(default) {
            return Err(format!("default must be {}", param.kind.describe()));
        }
        if param
            .choices
            .as_ref()
            .is_some_and(|choices| !choices.contains(default))
        {
            return Err("default must be one of the enum values".into());
        }
    }
    Ok(())
}

fn input_schema(params: &BTreeMap<String, Param>) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for (name, param) in params {
        let mut property = Map::new();
        property.insert("type".into(), json!(param.kind.as_str()));
        if param.kind == ParamType::Array {
            property.insert("items".into(), json!({"type": "string"}));
        }
        if let Some(description) = param
            .description
            .as_deref()
            .map(str::trim)
            .filter(|description| !description.is_empty())
        {
            property.insert("description".into(), json!(description));
        }
        if let Some(choices) = &param.choices {
            property.insert("enum".into(), json!(choices));
        }
        if let Some(default) = &param.default {
            property.insert("default".into(), default.clone());
        }
        properties.insert(name.clone(), Value::Object(property));
        if param.is_required() {
            required.push(name.clone());
        }
    }
    let mut schema = json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false,
    });
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema
}

fn build(name: &str, manifest: Manifest, scope: Scope, path: PathBuf) -> Result<ToolEntry, String> {
    validate(&manifest)?;
    let spec = ToolSpec {
        name: name.to_string(),
        description: manifest.description.trim().to_string(),
        input_schema: input_schema(&manifest.params),
    };
    let tool = CustomTool {
        scope,
        path,
        params: manifest.params,
        command: manifest.command,
        timeout: Duration::from_secs(manifest.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS)),
    };
    Ok(ToolEntry::new(spec, ToolImpl::Custom(tool)))
}

fn load(path: &Path, scope: Scope) -> Result<ToolEntry, String> {
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| valid_tool_name(stem))
        .ok_or("file name must be 1-64 ASCII letters, digits, '_' or '-' followed by .json")?;
    let metadata = std::fs::metadata(path).map_err(|error| format!("cannot read: {error}"))?;
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(format!(
            "manifest is larger than {} KiB",
            MAX_MANIFEST_BYTES / 1024
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|error| format!("cannot read: {error}"))?;
    let manifest: Manifest =
        serde_json::from_str(&text).map_err(|error| format!("invalid manifest: {error}"))?;
    build(name, manifest, scope, path.to_path_buf())
}

type Stamp = (Option<SystemTime>, u64);

fn stamp(path: &Path) -> Stamp {
    std::fs::metadata(path).map_or((None, 0), |metadata| {
        (metadata.modified().ok(), metadata.len())
    })
}

/// Manifests `define_tool` saved during this process. They load even while
/// project tools are untrusted, because this user's own agent wrote them.
/// Each path keeps the texts written to it, so a file something else later
/// rewrites stops loading, while `/undo` back to an earlier saved version
/// still does.
#[derive(Clone, Default)]
pub(super) struct Authored(Arc<Mutex<HashMap<PathBuf, Vec<String>>>>);

impl Authored {
    fn insert(&self, path: PathBuf, text: String) {
        if let Ok(mut paths) = self.0.lock() {
            let texts = paths.entry(path).or_default();
            if !texts.contains(&text) {
                texts.push(text);
            }
        }
    }

    /// Whether `path` still holds text this process wrote. Only paths it
    /// wrote are read.
    fn wrote(&self, path: &Path) -> bool {
        let Ok(paths) = self.0.lock() else {
            return false;
        };
        let Some(texts) = paths.get(path) else {
            return false;
        };
        std::fs::metadata(path).is_ok_and(|metadata| metadata.len() <= MAX_MANIFEST_BYTES)
            && std::fs::read_to_string(path).is_ok_and(|current| texts.contains(&current))
    }
}

/// Parsed manifests keyed by path and reused while size and modification
/// time hold. Entries are shared as `Arc`s so unchanged scans keep the same
/// tool specs and token estimates.
#[derive(Default)]
pub(super) struct ManifestCache {
    entries: HashMap<PathBuf, (Stamp, Result<Arc<ToolEntry>, String>)>,
    /// Whether each executable implements the removed `--describe` protocol.
    legacy: HashMap<PathBuf, (Stamp, bool)>,
    authored: Authored,
}

impl ManifestCache {
    fn is_legacy_tool(&mut self, path: &Path) -> bool {
        let current = stamp(path);
        if let Some((cached, legacy)) = self.legacy.get(path)
            && *cached == current
        {
            return *legacy;
        }
        let legacy = mentions_describe(path);
        self.legacy.insert(path.to_path_buf(), (current, legacy));
        legacy
    }
}

/// Old exec tools had to answer `--describe`, so their script or binary
/// contains that text. Helper scripts kept next to manifests do not.
fn mentions_describe(path: &Path) -> bool {
    const NEEDLE: &[u8] = b"--describe";
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_LEGACY_SCAN_BYTES).read_to_end(&mut bytes))
        .is_ok_and(|_| bytes.windows(NEEDLE.len()).any(|window| window == NEEDLE))
}

/// Loads one tools directory. Untrusted directories expose only manifests
/// this process authored. Executables that still implement the removed
/// `--describe` protocol produce a migration hint unless a manifest already
/// runs them.
pub(super) fn scan_dir(
    directory: &Path,
    scope: Scope,
    trusted: bool,
    cache: &mut ManifestCache,
) -> (Vec<Arc<ToolEntry>>, Vec<String>) {
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    let Ok(listing) = std::fs::read_dir(directory) else {
        cache
            .entries
            .retain(|path, _| path.parent() != Some(directory));
        cache
            .legacy
            .retain(|path, _| path.parent() != Some(directory));
        return (entries, warnings);
    };
    let mut manifests = Vec::new();
    let mut executables = Vec::new();
    for path in listing.flatten().map(|entry| entry.path()) {
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            if path.is_file() {
                manifests.push(path);
            }
        } else if is_executable_file(&path) {
            executables.push(path);
        }
    }
    manifests.sort();
    executables.sort();
    cache.entries.retain(|path, _| {
        path.parent() != Some(directory) || manifests.binary_search(path).is_ok()
    });
    cache.legacy.retain(|path, _| {
        path.parent() != Some(directory) || executables.binary_search(path).is_ok()
    });
    for path in manifests {
        if !trusted && !cache.authored.wrote(&path) {
            continue;
        }
        let current = stamp(&path);
        let result = match cache.entries.get(&path) {
            Some((cached, result)) if *cached == current => result.clone(),
            _ => {
                let result = load(&path, scope).map(Arc::new);
                cache
                    .entries
                    .insert(path.clone(), (current, result.clone()));
                result
            }
        };
        match result {
            Ok(entry) => entries.push(entry),
            Err(reason) => warnings.push(format!("{}: {reason}", path.display())),
        }
    }
    if trusted {
        for path in executables {
            let file = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
            let Some(file) = file else {
                continue;
            };
            let referenced = entries.iter().any(|entry| {
                matches!(&entry.imp, ToolImpl::Custom(tool) if tool.command.contains(&file))
            });
            if !referenced && cache.is_legacy_tool(&path) {
                let stem = path
                    .file_stem()
                    .map_or_else(|| file.clone(), |stem| stem.to_string_lossy().into_owned());
                warnings.push(format!(
                    "{}: executable tools now need a JSON manifest because Yawl no longer runs \
                     `--describe`; keep this one by adding {stem}.json with \"command\": \
                     \"$YAWL_TOOL_DIR/{file}\" (arguments still arrive as JSON on stdin)",
                    path.display()
                ));
            }
        }
    }
    (entries, warnings)
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Whether `./.yawl/tools/` holds manifests, which makes the project's
/// trust decision matter.
pub fn has_project_tools(config: &Config) -> bool {
    std::fs::read_dir(config.project_dir.join("tools")).is_ok_and(|listing| {
        listing.flatten().any(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
        })
    })
}

pub(super) fn define_entry() -> ToolEntry {
    ToolEntry::new(
        ToolSpec {
            name: DEFINE_TOOL_NAME.into(),
            description: "Save a reusable tool that persists across sessions and is callable from your next step. Use it when the user asks for a repeatable command or workflow, or when you notice one they will need again. Redefine a name to update it. Create and update tools only through this tool; it validates the definition and reports problems.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Tool name: 1-64 ASCII letters, digits, '_' or '-'; snake_case reads best"},
                    "description": {"type": "string", "description": "What the tool does and when to use it; this is all you will see in later sessions"},
                    "command": {"type": "string", "description": "Script for `sh -c` in the working directory. {{param}} expands to that argument already shell-quoted, so never wrap it in quotes. Arguments also arrive as a JSON object on stdin; $YAWL_TOOL_DIR is the manifest's directory. Exit nonzero on failure."},
                    "params": {
                        "type": "object",
                        "description": "Parameters by name. Each is required unless it sets required=false, a default, or a flag.",
                        "additionalProperties": {
                            "type": "object",
                            "properties": {
                                "type": {"type": "string", "enum": ["string", "integer", "number", "boolean", "array", "object"], "description": "array means an array of strings"},
                                "description": {"type": "string"},
                                "required": {"type": "boolean"},
                                "default": {"description": "Value used when the argument is omitted"},
                                "enum": {"type": "array", "description": "Allowed values"},
                                "flag": {"type": "string", "description": "Booleans only: text such as --release inserted when true; false inserts nothing"}
                            },
                            "required": ["type"]
                        }
                    },
                    "scope": {"type": "string", "enum": ["project", "global"], "description": "project: this repository only; global: every project for this user"},
                    "timeout_secs": {"type": "integer", "minimum": 1, "description": "Default 120"}
                },
                "required": ["name", "description", "command", "scope"]
            }),
        },
        ToolImpl::DefineTool,
    )
}

/// Everything `define_tool` needs from the scan that advertised it.
pub(super) struct Toolsmith {
    global_dir: PathBuf,
    project_dir: PathBuf,
    home_dir: PathBuf,
    project_trusted: bool,
    web_browsing: bool,
    authored: Authored,
}

impl Toolsmith {
    pub(super) fn new(config: &Config, cache: &ManifestCache) -> Toolsmith {
        let [global_dir, project_dir] = config.tool_dirs();
        Toolsmith {
            global_dir,
            project_dir,
            home_dir: config.home_dir.clone(),
            project_trusted: config.project_skills_trusted(),
            web_browsing: config.web_browsing,
            authored: cache.authored.clone(),
        }
    }

    fn directory(&self, scope: Scope) -> &Path {
        match scope {
            Scope::Global => &self.global_dir,
            Scope::Project => &self.project_dir,
        }
    }

    /// The manifest a `define_tool` call would write, so `/undo` can
    /// snapshot it before the call runs.
    pub(super) fn target(&self, args: &Value) -> Option<PathBuf> {
        let name = args.get("name")?.as_str()?.trim();
        let scope = parse_scope(args.get("scope")).ok()?;
        valid_tool_name(name).then(|| self.directory(scope).join(format!("{name}.json")))
    }

    pub(super) fn define(&self, args: &Value) -> ToolOutcome {
        match self.try_define(args) {
            Ok(report) => ToolOutcome::ok(report),
            Err(error) => ToolOutcome::error(format!("Tool not saved: {error}")),
        }
    }

    fn try_define(&self, args: &Value) -> Result<String, String> {
        let object = args.as_object().ok_or("arguments must be a JSON object")?;
        if let Some(unknown) = object
            .keys()
            .find(|key| !DEFINE_FIELDS.contains(&key.as_str()))
        {
            return Err(format!(
                "unknown field '{unknown}'; use {}",
                DEFINE_FIELDS.join(", ")
            ));
        }
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .ok_or("name is required")?;
        if !valid_tool_name(name) {
            return Err("name must be 1-64 ASCII letters, digits, '_' or '-'".into());
        }
        if super::reserved_tool_name(self.web_browsing, name) {
            return Err(format!(
                "'{name}' is a builtin tool name; choose a different name"
            ));
        }
        let scope = parse_scope(object.get("scope"))?;
        let mut manifest = Map::new();
        for key in ["description", "command", "params", "timeout_secs"] {
            let Some(value) = object.get(key).filter(|value| !value.is_null()) else {
                continue;
            };
            // Some models send nested objects as JSON text; accept that when
            // it is unambiguous rather than failing the whole definition.
            let value = match (key, value) {
                ("params", Value::String(text)) => serde_json::from_str::<Value>(text)
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| value.clone()),
                ("timeout_secs", Value::String(text)) => text
                    .trim()
                    .parse::<u64>()
                    .map_or_else(|_| value.clone(), Value::from),
                _ => value.clone(),
            };
            manifest.insert(key.into(), value);
        }
        let manifest: Manifest =
            serde_json::from_value(Value::Object(manifest)).map_err(|error| error.to_string())?;
        if scope == Scope::Project && !self.project_trusted && self.project_denied() {
            return Err(
                "this project is marked untrusted, so project tools never load here. Use \
                 scope \"global\", or ask the user to trust the project with \
                 `yawl --trust-project`"
                    .into(),
            );
        }
        let directory = self.directory(scope);
        let path = directory.join(format!("{name}.json"));
        let mut text = serde_json::to_string_pretty(&manifest)
            .map_err(|error| format!("cannot encode manifest: {error}"))?;
        text.push('\n');
        let usage = signature(name, &manifest.params);
        build(name, manifest, scope, path.clone())?;
        let existed = path.is_file();
        write_atomically(directory, &path, &text)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        self.authored.insert(path.clone(), text);

        let verb = if existed { "Updated" } else { "Created" };
        let mut report = format!(
            "{verb} {} tool `{name}` at {}.\nFrom your next step on, call {usage} instead of running its command through shell.\nIt persists across sessions; delete the file to remove it.",
            scope.label(),
            path.display(),
        );
        let other = match scope {
            Scope::Global => Scope::Project,
            Scope::Project => Scope::Global,
        };
        if self.directory(other).join(format!("{name}.json")).is_file() {
            report.push_str(match scope {
                Scope::Project => "\nIt overrides the global tool with the same name here.",
                Scope::Global => {
                    "\nA project tool with the same name takes precedence in this project."
                }
            });
        }
        if scope == Scope::Project && !self.project_trusted {
            report.push_str(
                "\nThis project is not trusted yet, so in later sessions the tool loads after \
                 the user trusts the project.",
            );
        }
        Ok(report)
    }

    fn project_denied(&self) -> bool {
        let root = crate::trust::project_root(&crate::config::working_dir());
        matches!(
            crate::trust::load(&self.home_dir, &root),
            Ok(crate::trust::Decision::Denied)
        )
    }
}

fn parse_scope(value: Option<&Value>) -> Result<Scope, String> {
    match value.and_then(Value::as_str) {
        Some("project") => Ok(Scope::Project),
        Some("global") => Ok(Scope::Global),
        _ => {
            Err("scope must be \"project\" (this repository) or \"global\" (every project)".into())
        }
    }
}

/// Writes through a hidden temporary file and renames it into place, so a
/// concurrent scan never parses a half-written manifest.
fn write_atomically(directory: &Path, path: &Path, text: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)?;
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = directory.join(format!(".{file_name}.{}.tmp", std::process::id()));
    if let Err(error) = std::fs::write(&temporary, text) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{CatalogCache, Registry};

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "yawl-custom-{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            let _ = std::fs::remove_dir_all(&root);
            Self(root)
        }

        fn config(&self) -> Config {
            Config {
                model: Some("test".into()),
                home_dir: self.0.join("home"),
                project_dir: self.0.join("project"),
                ..Config::test_default()
            }
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tool(manifest: Value) -> Result<CustomTool, String> {
        let manifest: Manifest =
            serde_json::from_value(manifest).map_err(|error| error.to_string())?;
        let entry = build(
            "probe",
            manifest,
            Scope::Global,
            PathBuf::from("/tmp/probe.json"),
        )?;
        match entry.imp {
            ToolImpl::Custom(tool) => Ok(tool),
            _ => Err("not a custom tool".into()),
        }
    }

    fn main_registry(config: &Config, cache: &mut CatalogCache) -> Registry {
        let mut registry = Registry::scan(config, cache);
        registry.advertise_define_tool(config, cache);
        registry
    }

    #[test]
    fn placeholders_pass_hostile_values_as_single_literal_arguments() {
        let tool = tool(json!({
            "description": "echo arguments one per line",
            "params": {
                "text": {"type": "string"},
                "items": {"type": "array", "required": false},
                "count": {"type": "integer", "default": 2},
                "loud": {"type": "boolean", "flag": "--loud"},
                "missing": {"type": "string", "required": false}
            },
            "command": "printf '<%s>\\n' {{text}} {{items}} {{count}} {{loud}} {{missing}}"
        }))
        .expect("valid tool");
        let hostile = "it's $(touch pwned); `id` \"quoted\" * ~";
        let outcome = tool.run(
            "probe",
            &json!({"text": hostile, "items": ["a b", 7], "loud": true}),
            "session",
        );
        assert!(!outcome.is_error, "{}", outcome.content);
        assert_eq!(
            outcome.content,
            format!("<{hostile}>\n<a b>\n<7>\n<2>\n<--loud>\n")
        );

        let quiet = tool.run("probe", &json!({"text": ""}), "session");
        assert_eq!(quiet.content, "<>\n<2>\n");
    }

    #[test]
    fn arguments_are_coerced_checked_and_sent_on_stdin() {
        let tool = tool(json!({
            "description": "read stdin",
            "params": {
                "mode": {"type": "string", "enum": ["fast", "full"], "default": "fast"},
                "retries": {"type": "integer"},
                "paths": {"type": "array"},
                "dry_run": {"type": "boolean", "required": false}
            },
            "command": "cat"
        }))
        .expect("valid tool");
        let outcome = tool.run(
            "probe",
            &json!({"retries": "3", "paths": "src", "dry_run": "true"}),
            "session",
        );
        assert!(!outcome.is_error, "{}", outcome.content);
        assert_eq!(
            serde_json::from_str::<Value>(&outcome.content).expect("stdin json"),
            json!({"mode": "fast", "retries": 3, "paths": ["src"], "dry_run": true})
        );

        for (args, expected) in [
            (json!({"paths": []}), "missing required argument 'retries'"),
            (
                json!({"retries": 1, "paths": [], "mode": "slow"}),
                "argument 'mode' must be one of \"fast\", \"full\"",
            ),
            (
                json!({"retries": 1.5, "paths": []}),
                "argument 'retries' must be an integer",
            ),
            (
                json!({"retries": 1, "paths": [{"nested": true}]}),
                "argument 'paths' must be an array of strings",
            ),
            (json!(["positional"]), "arguments must be a JSON object"),
        ] {
            let outcome = tool.run("probe", &args, "session");
            assert!(outcome.is_error);
            assert!(outcome.content.contains(expected), "{}", outcome.content);
            assert!(
                outcome.content.contains(
                    "Usage: probe(dry_run?: boolean, mode?: string, paths: array, retries: integer)"
                ),
                "{}",
                outcome.content
            );
        }
    }

    #[test]
    fn whole_numbers_stay_integral_for_enums_and_expansion() {
        let tool = tool(json!({
            "description": "print a ratio",
            "params": {"ratio": {"type": "number", "enum": [1, 2.5]}},
            "command": "printf '%s' {{ratio}}"
        }))
        .expect("valid tool");
        for (ratio, expected) in [
            (json!("1"), "1"),
            (json!(1.0), "1"),
            (json!(1), "1"),
            (json!("2.5"), "2.5"),
        ] {
            let outcome = tool.run("probe", &json!({"ratio": ratio}), "session");
            assert!(!outcome.is_error, "{ratio}: {}", outcome.content);
            assert_eq!(outcome.content, expected, "{ratio}");
        }
        let outcome = tool.run("probe", &json!({"ratio": "3"}), "session");
        assert!(
            outcome.content.contains("must be one of 1, 2.5"),
            "{}",
            outcome.content
        );
    }

    #[test]
    fn validation_explains_unsafe_or_ambiguous_manifests() {
        for (manifest, expected) in [
            (
                json!({"description": "x", "parameters": {}, "command": "true"}),
                "unknown field `parameters`",
            ),
            (
                json!({"description": "x", "command": "echo {{name}}"}),
                "params does not declare 'name'",
            ),
            (
                json!({"description": "x", "params": {"name": {"type": "string"}}, "command": "echo \"{{name}}\""}),
                "{{name}} is inside quotes",
            ),
            (
                json!({"description": "x", "params": {"name": {"type": "string"}}, "command": "echo 'hi {{ name }}'"}),
                "{{name}} is inside quotes",
            ),
            (
                json!({"description": "x", "params": {"name": {"type": "string", "flag": "--x"}}, "command": "true"}),
                "flag is only valid for boolean params",
            ),
            (
                json!({"description": "x", "params": {"n": {"type": "integer", "enum": ["1"]}}, "command": "true"}),
                "enum value \"1\" is not an integer",
            ),
            (
                json!({"description": "x", "params": {"n": {"type": "string", "enum": ["a"], "default": "b"}}, "command": "true"}),
                "default must be one of the enum values",
            ),
            (
                json!({"description": "x", "params": {"n": {"type": "string", "required": true, "default": "a"}}, "command": "true"}),
                "cannot also be required",
            ),
            (
                json!({"description": "x", "params": {"bad-name": {"type": "string"}}, "command": "true"}),
                "param 'bad-name'",
            ),
            (
                json!({"description": "x", "params": {"n": {"type": "text"}}, "command": "true"}),
                "unknown variant `text`",
            ),
            (
                json!({"description": " ", "command": "true"}),
                "description must say",
            ),
            (
                json!({"description": "x", "command": "true", "timeout_secs": 0}),
                "timeout_secs must be between",
            ),
            (
                json!({"description": "x", "command": "  "}),
                "command must not be empty",
            ),
        ] {
            let error = tool(manifest.clone()).err().unwrap_or_default();
            assert!(error.contains(expected), "{manifest}: {error}");
        }
    }

    #[test]
    fn helper_scripts_resolve_after_changing_directory() {
        let root = TempRoot::new("tool-dir");
        let tools = root.0.join("tools");
        std::fs::create_dir_all(&tools).expect("tools dir");
        std::fs::write(tools.join("helper.sh"), "printf helped").expect("helper");
        let relative = Path::new("tools/probe.json");
        let entry = build(
            "probe",
            serde_json::from_value(json!({
                "description": "call a helper after changing directory",
                "command": "cd / && sh \"$YAWL_TOOL_DIR/helper.sh\""
            }))
            .expect("manifest"),
            Scope::Project,
            root.0.join(relative),
        )
        .expect("valid tool");
        let ToolImpl::Custom(tool) = entry.imp else {
            panic!("custom tool expected");
        };
        let outcome = tool.run("probe", &json!({}), "session");
        assert_eq!(outcome.content, "helped");
    }

    #[test]
    fn readme_example_manifest_is_valid() {
        let readme = include_str!("../../README.md");
        let start = readme
            .find("```json\n{\n  \"description\": \"Run the test suite")
            .expect("example");
        let example = &readme[start + "```json\n".len()..];
        let example = &example[..example.find("```").expect("closing fence")];
        let tool =
            tool(serde_json::from_str(example).expect("example json")).expect("valid example");
        assert_eq!(
            tool.expand(
                &tool
                    .normalize(&json!({"release": true}))
                    .expect("arguments")
            ),
            "cargo test --release -- "
        );
        assert_eq!(
            tool.expand(
                &tool
                    .normalize(&json!({"filter": "parse args"}))
                    .expect("arguments")
            ),
            "cargo test  -- 'parse args'"
        );
    }

    #[test]
    fn shell_commands_a_tool_wraps_point_at_the_tool() {
        let entry = |name: &str, command: &str| {
            let manifest = json!({
                "description": "d",
                "params": {"filter": {"type": "string", "required": false}},
                "command": command
            });
            Arc::new(
                build(
                    name,
                    serde_json::from_value(manifest).expect("manifest"),
                    Scope::Global,
                    PathBuf::from(format!("/tmp/{name}.json")),
                )
                .expect("valid tool"),
            )
        };
        let entries = [
            entry("generic", "git {{filter}}"),
            entry("run_tests", "cargo test {{filter}}"),
            entry("lint", "npm run lint"),
        ];
        let hint = |command: &str| shell_hint(&entries, command);

        assert!(
            hint("  cargo test -p yawl parser")
                .is_some_and(|hint| hint.contains("custom tool `run_tests` wraps this command"))
        );
        assert!(hint("npm run lint").is_some_and(|hint| hint.contains("`lint`")));
        assert_eq!(
            hint("npm run lint -- --fix"),
            None,
            "a tool without placeholders cannot take the extra arguments"
        );
        assert_eq!(hint("cargo testing"), None);
        assert_eq!(
            hint("git status"),
            None,
            "single-word heads are too generic"
        );
        assert_eq!(hint("npm run lint:fix"), None);
    }

    #[test]
    fn non_parameter_braces_stay_literal() {
        let tool = tool(json!({
            "description": "go template",
            "command": "printf '%s' '{{.State.Status}}' && printf ' {{ }}'"
        }))
        .expect("go templates are not placeholders");
        let outcome = tool.run("probe", &json!({}), "session");
        assert_eq!(outcome.content, "{{.State.Status}} {{ }}");
    }

    #[test]
    fn scans_reuse_unchanged_manifests_and_explain_broken_files() {
        let root = TempRoot::new("scan");
        let directory = root.0.join("tools");
        std::fs::create_dir_all(&directory).expect("tools dir");
        std::fs::write(
            directory.join("good.json"),
            r#"{"description": "good", "command": "\"$YAWL_TOOL_DIR/helper\""}"#,
        )
        .expect("good manifest");
        std::fs::write(directory.join("broken.json"), r#"{"command": "true"}"#)
            .expect("broken manifest");
        for (executable, script) in [
            ("helper", "#!/bin/sh\necho hi\n"),
            ("lib.sh", "#!/bin/sh\necho shared helper\n"),
            (
                "legacy",
                "#!/bin/sh\n[ \"$1\" = --describe ] && echo '{}'\n",
            ),
        ] {
            let path = directory.join(executable);
            std::fs::write(&path, script).expect("script");
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .expect("chmod");
        }
        let mut cache = ManifestCache::default();

        let (first, warnings) = scan_dir(&directory, Scope::Global, true, &mut cache);
        assert_eq!(first.len(), 1);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("broken.json: invalid manifest: missing field `description`"));
        assert!(warnings[1].contains("legacy: executable tools now need a JSON manifest"));
        assert!(warnings[1].contains("legacy.json"));

        let (second, _) = scan_dir(&directory, Scope::Global, true, &mut cache);
        assert!(Arc::ptr_eq(&first[0], &second[0]));

        std::fs::write(
            directory.join("good.json"),
            r#"{"description": "better", "command": "\"$YAWL_TOOL_DIR/helper\""}"#,
        )
        .expect("rewrite manifest");
        let (third, _) = scan_dir(&directory, Scope::Global, true, &mut cache);
        assert_eq!(third[0].spec.description, "better");

        std::fs::remove_dir_all(&directory).expect("remove tools");
        let (gone, warnings) = scan_dir(&directory, Scope::Global, true, &mut cache);
        assert!(gone.is_empty() && warnings.is_empty());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn define_tool_saves_a_working_tool_for_the_next_step() {
        let root = TempRoot::new("define");
        let config = Config {
            project_skills_trusted: true,
            ..root.config()
        };
        let mut cache = CatalogCache::default();
        let registry = main_registry(&config, &mut cache);
        let definition = json!({
            "name": "greet",
            "description": "Greet someone by name",
            "params": {"who": {"type": "string", "description": "Name to greet"}},
            "command": "printf 'hello %s' {{who}}",
            "scope": "project"
        });
        let target = registry
            .mutating_path(DEFINE_TOOL_NAME, &definition.to_string())
            .expect("undo target");
        assert_eq!(target, root.0.join("project/tools/greet.json"));
        let mut padded = definition.clone();
        padded["name"] = json!(" greet ");
        assert_eq!(
            registry.mutating_path(DEFINE_TOOL_NAME, &padded.to_string()),
            Some(target.clone()),
            "undo must snapshot the file a padded name saves to"
        );

        let created = registry.execute(DEFINE_TOOL_NAME, &definition.to_string(), "s");
        assert!(!created.is_error, "{}", created.content);
        assert!(created.content.starts_with("Created project tool `greet`"));
        assert!(
            created
                .content
                .contains("call greet(who: string) instead of running its command through shell")
        );
        assert!(target.is_file());

        let next = main_registry(&config, &mut cache);
        assert_eq!(next.personalization(), Some(vec!["greet".to_string()]));
        let greeting = next.execute("greet", r#"{"who":"Ada Lovelace"}"#, "s");
        assert_eq!(greeting.content, "hello Ada Lovelace");

        let mut global = definition.clone();
        global["scope"] = json!("global");
        let shadowed = next.execute(DEFINE_TOOL_NAME, &global.to_string(), "s");
        assert!(
            shadowed
                .content
                .contains("A project tool with the same name takes precedence"),
            "{}",
            shadowed.content
        );
        let updated = next.execute(DEFINE_TOOL_NAME, &definition.to_string(), "s");
        assert!(updated.content.starts_with("Updated project tool `greet`"));
        assert!(updated.content.contains("overrides the global tool"));
    }

    #[test]
    fn define_tool_reports_problems_without_saving() {
        let root = TempRoot::new("define-errors");
        let config = root.config();
        let mut cache = CatalogCache::default();
        let registry = main_registry(&config, &mut cache);
        let base =
            json!({"name": "probe", "description": "d", "command": "true", "scope": "global"});
        let with = |key: &str, value: Value| {
            let mut args = base.clone();
            args[key] = value;
            args
        };
        for (args, expected) in [
            (
                with("name", json!("shell")),
                "'shell' is a builtin tool name",
            ),
            (with("name", json!("has space")), "name must be 1-64"),
            (with("scope", json!("user")), "scope must be \"project\""),
            (with("parameters", json!({})), "unknown field 'parameters'"),
            (
                with("command", json!("echo \"{{x}}\"")),
                "params does not declare 'x'",
            ),
            (
                with("params", json!("{\"x\": {\"type\": \"string\"}}"))
                    .as_object()
                    .map(|object| {
                        let mut object = object.clone();
                        object.insert("command".into(), json!("echo '{{x}}'"));
                        Value::Object(object)
                    })
                    .unwrap_or_default(),
                "{{x}} is inside quotes",
            ),
        ] {
            let outcome = registry.execute(DEFINE_TOOL_NAME, &args.to_string(), "s");
            assert!(outcome.is_error, "{args}");
            assert!(outcome.content.starts_with("Tool not saved: "));
            assert!(outcome.content.contains(expected), "{}", outcome.content);
        }
        assert!(!root.0.join("home/tools").exists());

        let saved = registry.execute(
            DEFINE_TOOL_NAME,
            &with("timeout_secs", json!("30")).to_string(),
            "s",
        );
        assert!(!saved.is_error, "{}", saved.content);
        let manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(root.0.join("home/tools/probe.json")).expect("manifest"),
        )
        .expect("manifest json");
        assert_eq!(
            manifest,
            json!({"description": "d", "command": "true", "timeout_secs": 30})
        );
    }

    #[test]
    fn untrusted_projects_load_only_tools_this_process_defined() {
        let root = TempRoot::new("authored");
        let config = root.config();
        let tools = root.0.join("project/tools");
        std::fs::create_dir_all(&tools).expect("tools dir");
        std::fs::write(
            tools.join("planted.json"),
            r#"{"description": "planted by the repository", "command": "true"}"#,
        )
        .expect("planted manifest");
        assert!(has_project_tools(&config));
        let mut cache = CatalogCache::default();
        let registry = main_registry(&config, &mut cache);
        assert_eq!(registry.personalization(), Some(Vec::new()));

        let defined = registry.execute(
            DEFINE_TOOL_NAME,
            &json!({"name": "mine", "description": "d", "command": "echo mine", "scope": "project"})
                .to_string(),
            "s",
        );
        assert!(!defined.is_error, "{}", defined.content);
        assert!(defined.content.contains("This project is not trusted yet"));

        let next = main_registry(&config, &mut cache);
        assert_eq!(next.personalization(), Some(vec!["mine".to_string()]));
        assert!(next.execute("planted", "{}", "s").is_error);

        std::fs::write(
            tools.join("mine.json"),
            r#"{"description": "rewritten by something else", "command": "echo other"}"#,
        )
        .expect("rewrite manifest");
        let rewritten = main_registry(&config, &mut cache);
        assert_eq!(
            rewritten.personalization(),
            Some(Vec::new()),
            "a manifest rewritten outside define_tool needs trust again"
        );
    }

    #[test]
    fn denied_projects_refuse_project_tools() {
        let root = TempRoot::new("denied");
        let config = root.config();
        let project = crate::trust::project_root(&crate::config::working_dir());
        crate::trust::store(&config.home_dir, &project, false).expect("deny trust");
        let mut cache = CatalogCache::default();
        let registry = main_registry(&config, &mut cache);
        let refused = registry.execute(
            DEFINE_TOOL_NAME,
            &json!({"name": "mine", "description": "d", "command": "true", "scope": "project"})
                .to_string(),
            "s",
        );
        assert!(refused.is_error);
        assert!(
            refused.content.contains("marked untrusted"),
            "{}",
            refused.content
        );
        assert!(!root.0.join("project/tools/mine.json").exists());
    }
}
